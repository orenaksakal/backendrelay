// Local multi-app load runner for self-hosting/scale-local/stack.sh: creates N
// ephemeral apps with the production DAU fixture shape, seeds users-per-app
// identities per app, and drives one dau-worker per app through the scenario's
// steady/burst/recovery holds. Resource use is sampled per hold from /proc, and
// pg_stat_statements is reset per hold for per-op statement costs. Admin and
// refresh tokens stay in memory: nothing here writes them to the output.
//
// Usage: node local-scale-run.mjs --stack <stack.json> [--apps 10]
//   [--users-per-app 1000] [--steady 300] [--burst 60] [--recovery 90]
//   [--ramp-per-second 50] [--out <dir>]
// Exit: 0 passed, 1 failed, 2 invalid (generator saturated or could not measure).
import assert from 'node:assert/strict';
import {Worker} from 'node:worker_threads';
import {mkdir,readdir,readFile,writeFile} from 'node:fs/promises';
import {dirname,join,resolve} from 'node:path';
import {homedir} from 'node:os';
import {randomUUID} from 'node:crypto';
import {execFile} from 'node:child_process';
import {promisify} from 'node:util';
import {generatorWorkerEnvironment} from './generator-worker-env.mjs';
const run=promisify(execFile);
const flags=new Map();
for(let index=2;index<process.argv.length;index+=2){
 const name=process.argv[index],value=process.argv[index+1];
 assert(name?.startsWith('--')&&value!==undefined&&!value.startsWith('--'),`unexpected argument ${name??''}; expected --name value pairs`);
 flags.set(name.slice(2),value);
}
const known=new Set(['stack','apps','users-per-app','steady','burst','recovery','ramp-per-second','out']);
for(const name of flags.keys())assert(known.has(name),'unknown argument --'+name);
const numeric=(name,fallback)=>{if(!flags.has(name))return fallback;const value=Number(flags.get(name));assert(Number.isFinite(value),'--'+name+' must be a finite number');return value;};
const startedAt=new Date().toISOString();
const stateDir=process.env.SCALE_STATE_DIR||join(process.env.XDG_STATE_HOME||join(homedir(),'.local/state'),'instant-scale-local/current');
const stackPath=resolve(flags.get('stack')||join(stateDir,'stack.json'));
const stack=JSON.parse(await readFile(stackPath,'utf8'));
for(const key of ['origin_url','server_pid','postmaster_pid','database_url','postgres_bin','data_dir','generator_cpus','server_log','postgres_log'])assert(stack[key]!==undefined&&stack[key]!==null&&stack[key]!=='','stack.json is missing '+key);
const origin=new URL(stack.origin_url);
// The generator must only ever reach the disposable local server recorded in
// stack.json; session.mjs reads this at module load, so it is set before any
// generator module is imported and forwarded to every worker below.
assert(origin.hostname==='127.0.0.1'||origin.hostname==='localhost','stack origin must be loopback');
process.env.LOADTEST_API_ORIGIN=origin.origin;
const {localApps,localScenario,localPhaseFailures}=await import('./local-scale-model.mjs');
const {fixtureIdentityCapacity,fixtureRoomMembers}=await import('./fixture-rooms.mjs');
const {rowId,summary,profile}=await import('./model.mjs');
const {latencyAcceptance}=await import('./dau-model.mjs');
const scenario=localScenario({apps:numeric('apps',10),usersPerApp:numeric('users-per-app',1000),steadySeconds:numeric('steady',300),burstSeconds:numeric('burst',60),recoverySeconds:numeric('recovery',90),rampPerSecond:numeric('ramp-per-second',50)});
const appList=localApps(scenario.apps);
const stamp=startedAt.replaceAll(':','-').replace('.','-');
const directory=resolve(flags.get('out')||join(dirname(stackPath),'runs',`${stamp}-${scenario.apps}x${scenario.usersPerApp}`));
await mkdir(join(directory,'phases'),{recursive:true});
const save=async(name,value)=>writeFile(join(directory,name),JSON.stringify(value,null,2)+'\n',{mode:0o600});
const emit=value=>console.log(JSON.stringify({at:new Date().toISOString(),...value}));
const sleep=ms=>new Promise(resolve=>setTimeout(resolve,ms));
const average=values=>values.length?values.reduce((total,value)=>total+value,0)/values.length:null;
const peak=values=>values.length?Math.max(...values):null;
async function request(path,{method='GET',body,headers={},timeout=30000,attempts=5}={}){
 const url=path.startsWith('http')?path:origin.origin+path;
 for(let attempt=1;;attempt+=1){
  let response;
  try{response=await fetch(url,{method,headers:{'Content-Type':'application/json',...headers},body:body===undefined?undefined:JSON.stringify(body),signal:AbortSignal.timeout(timeout)});}
  catch(error){if(attempt>=attempts)throw error;await sleep(Math.min(8000,500*2**(attempt-1))+Math.floor(Math.random()*250));continue;}
  if(response.ok)return response.json();
  const status=response.status;await response.body?.cancel();
  // Bounded retry/backoff for local admission control or a restarting pool.
  if(![429,500,502,503,504].includes(status)||attempt>=attempts)throw new Error(`HTTP-${status}`);
  await sleep(Math.min(8000,500*2**(attempt-1))+Math.floor(Math.random()*250));
 }
}
async function healthy(){try{const response=await fetch(origin.origin+'/health/system',{signal:AbortSignal.timeout(3000)});await response.body?.cancel();return response.status===200;}catch{return false;}}
// The fixture schema mirrors dau-run.mjs: same entities and fields, id unique,
// id/ownerId/roomId/updatedAt indexed, updatedAt checked as number. The create
// path derives the id attribute itself (unique, unindexed), so readiness waits
// on id uniqueness and only requires an explicit index on the other three.
const schema={entities:{
 todos:{attrs:{id:{config:{unique:true,indexed:true}},ownerId:{config:{indexed:true}},title:{},completed:{},revision:{},updatedAt:{valueType:'number',config:{indexed:true}}}},
 messages:{attrs:{id:{config:{unique:true,indexed:true}},roomId:{config:{indexed:true}},authorId:{},memberIds:{},text:{},revision:{},updatedAt:{valueType:'number',config:{indexed:true}}}},
}};
// Copied verbatim from dau-run.mjs so local results stay comparable.
const rules={todos:{allow:{view:'auth.id != null && auth.id == data.ownerId',create:'auth.id != null && auth.id == data.ownerId',update:'auth.id != null && auth.id == data.ownerId && auth.id == newData.ownerId',delete:'auth.id != null && auth.id == data.ownerId'}},messages:{allow:{view:'auth.id != null && auth.id in data.memberIds',create:'auth.id != null && auth.id == data.authorId && auth.id in data.memberIds',update:'auth.id != null && auth.id == data.authorId && auth.id in data.memberIds && newData.memberIds == data.memberIds && newData.roomId == data.roomId && newData.authorId == data.authorId',delete:'auth.id != null && auth.id == data.authorId'}},$default:{allow:{$default:'false'}}};
const requiredAttributes=[['todos','id','unique?'],['todos','ownerId','index?'],['messages','roomId','index?'],['todos','updatedAt','index?'],['messages','updatedAt','index?'],['todos','updatedAt','checked-data-type'],['messages','updatedAt','checked-data-type']];
const schemaReady=metadata=>requiredAttributes.every(([entity,label,field])=>field==='checked-data-type'?metadata.blobs?.[entity]?.[label]?.[field]==='number':metadata.blobs?.[entity]?.[label]?.[field]===true);
assert(await healthy(),'stack server is not healthy at '+origin.origin);
const seedStart=Date.now();
const workloads=[];
emit({phase:'seeding',apps:scenario.apps,usersPerApp:scenario.usersPerApp,totalUsers:scenario.totalUsers,out:directory});
for(const app of appList){
 const created=await request('/dash/apps/ephemeral',{method:'POST',body:{title:`Local scale loadtest - ${app.key}`,schema,rules:{code:rules}},timeout:60000});
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
   const index=start+offset,response=await admin('refresh_tokens',{email:`local-${app.index}-${index}@example.invalid`},{timeout:20000});
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
await save('scenario.json',{...scenario,apps:appList.map(app=>app.key),seededApps:scenario.apps,seedingMs,out:directory,startedAt,stack:{origin_url:stack.origin_url,git_commit:stack.git_commit??null,sut_cpus:stack.sut_cpus,generator_cpus:stack.generator_cpus}});
emit({phase:'seeded',apps:workloads.length,seedingMs});

const clockTicks=await (async()=>{try{return Number((await run('getconf',['CLK_TCK'])).stdout.trim())||100;}catch{return 100;}})();
const pageBytes=4096;
async function processTable(){
 const names=(await readdir('/proc')).filter(name=>/^\d+$/.test(name)),table=new Map();
 await Promise.all(names.map(async name=>{
  try{
   const stat=await readFile(`/proc/${name}/stat`,'utf8'),tail=stat.slice(stat.lastIndexOf(')')+2).split(' ');
   table.set(Number(name),{ppid:Number(tail[1]),ticks:Number(tail[11])+Number(tail[12]),rssBytes:Number(tail[21])*pageBytes});
  }catch{}
 }));
 return table;
}
// Postgres keeps one process per backend, so per-hold CPU and connection counts
// come from the postmaster's /proc subtree. Client backends are the descendants
// whose process title carries a host(port) suffix; auxiliary processes do not.
async function postgresTree(table){
 const children=new Map();
 for(const[pid,row]of table){const siblings=children.get(row.ppid);siblings?siblings.push(pid):children.set(row.ppid,[pid]);}
 const total={rssBytes:0,processes:0,clientBackends:0},ticks=new Map(),pending=[],queue=[stack.postmaster_pid];
 while(queue.length){
  const pid=queue.pop(),row=table.get(pid);
  if(!row)continue;
  ticks.set(pid,row.ticks);total.rssBytes+=row.rssBytes;total.processes+=1;
  if(pid!==stack.postmaster_pid)pending.push(readFile(`/proc/${pid}/cmdline`,'utf8').then(title=>{if(title.includes('('))total.clientBackends+=1;},()=>{}));
  queue.push(...children.get(pid)||[]);
 }
 await Promise.all(pending);
 return {...total,ticks};
}
async function sampleProcesses(){
 const table=await processTable(),server=table.get(stack.server_pid),postgres=await postgresTree(table);
 return {atMs:Date.now(),serverTicks:server?.ticks??null,serverRssBytes:server?.rssBytes??null,postgresTicks:postgres.ticks,postgresRssBytes:postgres.rssBytes,postgresProcesses:postgres.processes,clientBackends:postgres.clientBackends,serverAlive:Boolean(server)};
}
// Per-pid deltas: backends enter and leave the postmaster subtree between
// samples, so a total-over-subtree delta would lose an exited backend's ticks
// (and can go negative when an idle pool connection is reaped).
function pidDelta(current,previous){let delta=0;for(const[pid,ticks]of current){const before=previous.get(pid);if(before!==undefined&&ticks>=before)delta+=ticks-before;}return delta;}
// One sample per second for the length of a hold. The first observation only
// seeds the CPU delta, so the first reported core count is a one-second average.
function startSampling(){
 const samples=[];
 let active=true;
 const loop=(async()=>{
  let previous=null,serverTotal=0,postgresTotal=0,startedAt=null,next=Date.now()+1000;
  while(active){
   const sample=await sampleProcesses().catch(()=>null);
   if(sample){
    if(!startedAt)startedAt=sample.atMs;
    const row={at:new Date(sample.atMs).toISOString(),serverRssBytes:sample.serverRssBytes,postgresRssBytes:sample.postgresRssBytes,postgresProcesses:sample.postgresProcesses,clientBackends:sample.clientBackends,serverAlive:sample.serverAlive,serverCores:null,pgCores:null};
    if(previous){
     const seconds=Math.max(0.001,(sample.atMs-previous.atMs)/1000),serverDelta=sample.serverTicks!==null&&previous.serverTicks!==null&&sample.serverTicks>=previous.serverTicks?sample.serverTicks-previous.serverTicks:null,postgresDelta=pidDelta(sample.postgresTicks,previous.postgresTicks);
     if(serverDelta!==null){serverTotal+=serverDelta;row.serverCores=Math.max(0,serverDelta/(clockTicks*seconds));}
     postgresTotal+=postgresDelta;row.pgCores=Math.max(0,postgresDelta/(clockTicks*seconds));
     samples.push(row);
    }
    previous=sample;
   }
   while(active&&Date.now()<next)await sleep(Math.min(200,Math.max(0,next-Date.now())));
   next+=1000;
  }
  return {samples,startedAt,serverCpuTicks:serverTotal,postgresCpuTicks:postgresTotal};
 })();
 return {async stop(){active=false;return loop;}};
}
const psqlPath=join(stack.postgres_bin,'psql');
function psql(sql,timeout=30000){return run(psqlPath,[stack.database_url,'-X','--no-psqlrc','-A','-t','-v','ON_ERROR_STOP=1','-c',sql],{timeout,maxBuffer:4_000_000}).then(result=>result.stdout.trim());}
const statementsSummarySql=`SELECT json_build_object('calls',coalesce(sum(calls),0),'execMs',coalesce(sum(total_exec_time),0),'planMs',coalesce(sum(total_plan_time),0),'statements',count(*),'clientBackends',(SELECT count(*) FROM pg_stat_activity WHERE backend_type='client backend')) FROM pg_stat_statements`;
const statementsTopSql=`SELECT coalesce(json_agg(row_to_json(top)),'[]'::json) FROM (SELECT calls,round(exec_ms::numeric,3) AS exec_ms,round(plan_ms::numeric,3) AS plan_ms,rows,query FROM (SELECT calls,total_exec_time AS exec_ms,total_plan_time AS plan_ms,rows,left(query,200) AS query FROM pg_stat_statements ORDER BY total_exec_time DESC LIMIT 15) ranked) top`;
async function statementsStart(){try{await psql('SELECT pg_stat_statements_reset()');return null;}catch{return 'reset-failed';}}
async function statementsEnd(){
 try{return {available:true,...JSON.parse(await psql(statementsSummarySql)),top:JSON.parse(await psql(statementsTopSql))};}
 catch{return {available:false};}
}

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
const phaseReports=[],resourceReports=[],statementReports=[];
for(const definition of scenario.phases){
 if(halting)break;
 const epoch=Date.now()+1000;
 current={...definition,epoch,holdAt:epoch+definition.rampSeconds*1000,endAt:epoch+(definition.rampSeconds+definition.holdSeconds)*1000};
 workers.forEach((worker,index)=>worker.postMessage({type:'phase',phase:current,target:current.targets[index]}));
 emit({phase:'phase-start',name:current.name,targets:current.targets,rampSeconds:current.rampSeconds,holdSeconds:current.holdSeconds,rateMultiplier:current.rateMultiplier??1});
 let sampler=null,statementsReset=null;
 while(!halting&&Date.now()<current.endAt){
  if(!sampler&&Date.now()>=current.holdAt){sampler=startSampling();statementsReset=statementsStart();}
  if(sampler&&Date.now()>=current.holdAt&&!invalid&&[...progress.values()].some(status=>status.eventLoopP99Ms>200))invalidate('generator-event-loop-saturation');
  await sleep(200);
 }
 const sampled=sampler?await sampler.stop():{samples:[],startedAt:null,serverCpuTicks:0,postgresCpuTicks:0},resources=sampled.samples,resetFailure=await statementsReset;
 const statements=await statementsEnd();
 if(resetFailure)statements.resetFailure=resetFailure;
 await sleep(profile.operationTimeoutMs+500);
 const statuses=await Promise.all(workers.map(worker=>snapshot(worker))).catch(()=>{invalidate('incomplete-worker-accounting');return [];});
 if(!statuses.length||statuses.length!==workers.length)invalidate('incomplete-worker-accounting');
 const failures=statuses.length===workers.length?localPhaseFailures(current,statuses,latencyAcceptance,appList):['incomplete worker accounting'];
 if(statuses.some(status=>status.eventLoopP99Ms>200))invalidate('generator-event-loop-saturation');
 const completedOps=statuses.reduce((total,status)=>total+status.metrics.read.count+status.metrics.write.count,0),holds=current.holdSeconds;
 const cores=resources.filter(row=>row.serverCores!==null&&row.pgCores!==null),serverCores=cores.map(row=>row.serverCores),pgCores=cores.map(row=>row.pgCores);
 const windowSeconds=sampled.startedAt&&cores.length?Math.max(0.001,(Date.parse(cores.at(-1).at)-sampled.startedAt)/1000):0;
 const serverCpuMs=windowSeconds?sampled.serverCpuTicks/clockTicks*1000:null,pgCpuMs=windowSeconds?sampled.postgresCpuTicks/clockTicks*1000:null;
 const perOp=value=>completedOps&&Number.isFinite(value)?value/completedOps:null;
 const phase={name:current.name,holdSeconds:holds,holdStartedAt:new Date(current.holdAt).toISOString(),holdEndedAt:new Date(current.endAt).toISOString(),targets:current.targets,rampSeconds:current.rampSeconds,rateMultiplier:current.rateMultiplier??1,passed:!halting&&failures.length===0,failures,completedOps,opsPerSecond:completedOps/holds,read:metric(statuses,'read'),write:metric(statuses,'write'),realtime:metric(statuses,'realtime'),serverCores:{average:average(serverCores),peak:peak(serverCores)},pgCores:{average:average(pgCores),peak:peak(pgCores)},serverCpuMsPerOp:perOp(serverCpuMs),pgCpuMsPerOp:perOp(pgCpuMs),statementsPerOp:perOp(statements.calls),execMsPerOp:perOp(statements.execMs),clientBackends:statements.available?statements.clientBackends:null,generatorEventLoopP99Ms:Math.max(0,...statuses.map(status=>status.eventLoopP99Ms))};
 phaseReports.push(phase);
 resourceReports.push({name:current.name,window:{startedAt:phase.holdStartedAt,endedAt:new Date(cores.length?Date.parse(cores.at(-1).at):current.endAt).toISOString(),seconds:windowSeconds},serverCores:phase.serverCores,pgCores:phase.pgCores,serverCpuMs,pgCpuMs,serverCpuMsPerOp:phase.serverCpuMsPerOp,pgCpuMsPerOp:phase.pgCpuMsPerOp,completedOps,samples:resources});
 statementReports.push({name:current.name,...statements,statementsPerOp:phase.statementsPerOp,execMsPerOp:phase.execMsPerOp});
 await save(`phases/${current.name}.json`,{name:current.name,kind:current.kind,targets:current.targets,rampSeconds:current.rampSeconds,holdSeconds:holds,rateMultiplier:current.rateMultiplier??1,holdStartedAt:phase.holdStartedAt,holdEndedAt:phase.holdEndedAt,passed:phase.passed,failures,completedOps,opsPerSecond:phase.opsPerSecond,serverCores:phase.serverCores,pgCores:phase.pgCores,serverCpuMsPerOp:phase.serverCpuMsPerOp,pgCpuMsPerOp:phase.pgCpuMsPerOp,statements,apps:statuses.map(({type,requestId,latencySamplesMs,canceledLatencySamplesMs,diagnosticTimings,diagnosticWireTimings,...rest})=>rest)});
 await save('resources.json',{clockTicks,clockTicksPerSecond:clockTicks,notes:['serverRssBytes is the server pid resident pages; postgresRssBytes sums the postmaster subtree and therefore over-counts shared_buffers','serverCores and pgCores are one-second utime+stime deltas summed per pid; CPU-ms per op spans the sampler baseline to the last hold sample'],phases:resourceReports});
 await save('pg-statements.json',{phases:statementReports});
 emit({phase:'phase-complete',name:current.name,passed:phase.passed,completedOps,opsPerSecond:Math.round(phase.opsPerSecond),serverCores:phase.serverCores,pgCores:phase.pgCores,statementsPerOp:phase.statementsPerOp,failures});
 if(failures.length)stop('phase-latency-or-correctness-gate');
}
finished=true;
for(const worker of workers)worker.postMessage({type:'stop'});
await Promise.all(workers.map(worker=>new Promise(resolve=>{worker.once('exit',resolve);setTimeout(()=>{worker.terminate();resolve();},15000).unref();})));
clearTimeout(watchdog);
const passed=!invalid&&phaseReports.length===scenario.phases.length&&phaseReports.every(report=>report.passed);
const result={passed,invalid:Boolean(invalid),invalidReason:invalid,haltReason,totalUsers:scenario.totalUsers,apps:scenario.apps,usersPerApp:scenario.usersPerApp,seedingMs,generatorCpus:stack.generator_cpus,origin:stack.origin_url,gitCommit:stack.git_commit??null,startedAt,finishedAt:new Date().toISOString(),out:directory,phases:phaseReports.map(({name,passed:phasePassed,failures,opsPerSecond,read,write,realtime,serverCores,pgCores,serverCpuMsPerOp,pgCpuMsPerOp,statementsPerOp,execMsPerOp,completedOps,clientBackends,generatorEventLoopP99Ms})=>({name,passed:phasePassed,failures,opsPerSecond,read,write,realtime,serverCores,pgCores,serverCpuMsPerOp,pgCpuMsPerOp,statementsPerOp,execMsPerOp,completedOps,clientBackends,generatorEventLoopP99Ms}))};
await save('result.json',result);
const cell=(value,width=9)=>String(value??'-').padStart(width),latency=value=>[value.p50,value.p95,value.p99,value.max].map(entry=>entry===null?'-':entry.toFixed(1)).join('/'),cores=value=>value.average===null?'-':`${value.average.toFixed(2)}/${value.peak===null?'-':value.peak.toFixed(2)}`;
console.log(['phase'.padEnd(9),'result'.padEnd(7),cell('ops/s',9),('read p50/p95/p99/max').padEnd(30),('write p50/p95/p99/max').padEnd(31),cell('srv cores',11),cell('pg cores',11),cell('srv ms/op',10),cell('pg ms/op',9),cell('stmt/op',8)].join(' '));
for(const phase of result.phases)console.log([phase.name.padEnd(9),(phase.passed?'PASS':'FAIL').padEnd(7),cell(phase.opsPerSecond.toFixed(1),9),latency(phase.read).padEnd(30),latency(phase.write).padEnd(31),cell(cores(phase.serverCores),11),cell(cores(phase.pgCores),11),cell(phase.serverCpuMsPerOp?.toFixed(2),10),cell(phase.pgCpuMsPerOp?.toFixed(2),9),cell(phase.statementsPerOp?.toFixed(2),8)].join(' '));
console.log(`result: ${result.invalid?'INVALID':result.passed?'PASSED':'FAILED'} ${phaseReports.filter(report=>report.passed).length}/${scenario.phases.length} phases passed${phaseReports.length<scenario.phases.length?` (${phaseReports.length}/${scenario.phases.length} ran)`:''}${result.invalidReason||result.haltReason?' ('+(result.invalidReason||result.haltReason)+')':''}: ${join(directory,'result.json')}`);
process.exit(result.invalid?2:result.passed?0:1);
