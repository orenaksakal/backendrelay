import {createHash} from 'node:crypto';
export const tiers=[200,500,1000,2500,5000,10000];
export const profile={name:'light-active',rowsPerUser:10,readIntervalMs:60000,writeIntervalMs:300000,
  operationTimeoutMs:10000,readP95Ms:750,readP99Ms:2000,writeP95Ms:1000,writeP99Ms:3000,
  realtimeP95Ms:2000,realtimeP99Ms:5000};
export function rowId(user,index){const h=createHash('sha256').update(`${user}:loadtest:${index}`).digest('hex');return `${h.slice(0,8)}-${h.slice(8,12)}-4${h.slice(13,16)}-a${h.slice(17,20)}-${h.slice(20,32)}`;}
export function summary(values){const sorted=[...values].sort((a,b)=>a-b);return {count:sorted.length,p50Ms:sorted[Math.ceil(sorted.length*.5)-1]??null,p95Ms:sorted[Math.ceil(sorted.length*.95)-1]??null,p99Ms:sorted[Math.ceil(sorted.length*.99)-1]??null,maxMs:sorted.at(-1)??null};}
export function verdict(report){
  const failures=[];
  if(report.aborted)failures.push(report.abortReason);
  if(report.connectedAtHold<report.users*.99)failures.push('connection target not reached');
  if(report.unexpectedWireErrors||report.subscriptionErrors||report.disconnects)failures.push('unexpected realtime errors or disconnects');
  if(report.generatorEventLoopP99Ms>200)failures.push('generator event loop saturation');
  for(const name of ['read','write','realtime']){
    const metric=report.metrics[name];
    if(!metric?.count)failures.push(`${name} has no successful observations`);
    if(!Number.isFinite(metric?.p95Ms)||!Number.isFinite(metric?.p99Ms))failures.push(`${name} latency is unmeasured`);
    if(metric?.failed)failures.push(`${name} failures`);
    if(metric?.p95Ms>profile[name+'P95Ms']||metric?.p99Ms>profile[name+'P99Ms'])failures.push(`${name} latency gate`);
  }
  const expectedReads=report.users*report.holdSeconds/60,expectedWrites=report.users*report.holdSeconds/300;
  if(report.metrics.read.count<expectedReads*.90||report.metrics.write.count<expectedWrites*.85)failures.push('offered workload not delivered');
  return {passed:failures.length===0,failures};
}
