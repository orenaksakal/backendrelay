export const apps=[
 {key:'tasks',title:'Tasks',readMs:30000,writeMs:120000,sharedWrite:.5,append:false},
 {key:'feed',title:'Feed',readMs:20000,writeMs:300000,sharedWrite:.7,append:true},
 {key:'analytics',title:'Analytics',readMs:15000,writeMs:600000,sharedWrite:.1,append:false},
 {key:'chat',title:'Chat',readMs:20000,writeMs:30000,sharedWrite:1,append:true},
 {key:'mobile',title:'Mobile',readMs:45000,writeMs:180000,sharedWrite:.2,append:false},
];
// User-approved on 2026-10-19: a hard maximum, not a percentile allowance.
// The historical single-app profile and archived v1 receipts remain unchanged.
export const latencyAcceptance=Object.freeze({version:2,criterion:'observed-maximum',readMaxMs:3000,writeMaxMs:3000,realtimeMaxMs:3000});
export function scenario(dau){
 if(![1000,5000,10000].includes(dau))throw new Error('DAU must be 1000, 5000 or 10000 per app');
 const busy=Math.ceil(dau/36),quiet=Math.ceil(dau/240);
 const phase=(name,factors,seconds,kind)=>({name,targets:factors.map(n=>n*busy),holdSeconds:seconds,rampSeconds:45,kind});
 return {dauPerApp:dau,totalProjectDAU:dau*5,groupSize:20,poolPerApp:busy*4,
  assumptions:{sessionsPerDAU:2,meanSessionMinutes:10,quietHours:16,busyHours:6,spikyHours:2,
   calibrationScope:'Twenty active minutes per DAU is the project-wide average before rounding, not identical time in every app. The app-specific spike mix gives Tasks longer daily activity than Mobile.',
   modeledDailyActiveMinutesPerDAU:Object.fromEntries(apps.map((app,index)=>[app.key,(quiet*16*60+busy*6*60+busy*2*60*[[4,1,1,1,1],[4,3,1,1,1],[2,2,3,1,1],[1,1,2,3,2]].reduce((n,v)=>n+v[index],0)/4)/dau])),
   formula:'busy concurrency per app = DAU/36; quiet = DAU/240; spikes average 1.8x busy across all five apps. Weighted daily session time is 20 minutes/user/day before integer rounding.',
   note:'DAU is a traffic model, not simultaneous users; identity pool covers peak concurrency, not every daily unique account. Session lifetimes churn over 5–15 minutes; daily calibration is an average, not an exact two-session ledger for each app/user.'},
  phases:[{name:'quiet',targets:apps.map(()=>quiet),holdSeconds:120,rampSeconds:30,kind:'quiet'},
   phase('busy',[1,1,1,1,1],120,'busy'),
    phase('tasks-spike',[4,1,1,1,1],600,'spike'),
    phase('overlapping-tasks-feed',[4,3,1,1,1],600,'spike'),
   phase('feed-analytics-wave',[2,2,3,1,1],120,'spike'),
   phase('chat-mobile-wave',[1,1,2,3,2],180,'spike'),
   phase('busy-recovery',[1,1,1,1,1],120,'busy'),
   {name:'quiet-recovery',targets:apps.map(()=>quiet),holdSeconds:120,rampSeconds:45,kind:'quiet'}]};
}
export function random(seed){return ()=>{seed|=0;seed=seed+0x6D2B79F5|0;let t=Math.imul(seed^seed>>>15,1|seed);t=t+Math.imul(t^t>>>7,61|t)^t;return ((t^t>>>14)>>>0)/4294967296;};}
export function phaseFailures(phase,statuses,limits,appList=apps){
 const failures=[];
 if(statuses.length!==appList.length||new Set(statuses.map(s=>s.app)).size!==appList.length)return ['incomplete '+appList.length+'-app accounting'];
 for(let i=0;i<appList.length;i++){
  const app=appList[i],s=statuses.find(s=>s.app===app.key),target=phase.targets[i];
  if(!s){failures.push(app.key+': missing worker');continue;}
   if(!s.concurrency?.samples||s.concurrency.peak<target||s.concurrency.mean<target*.98)failures.push(app.key+': connection target/exposure');
   if(phase.requireObserverExposure&&(!(s.concurrency?.samples>=phase.holdSeconds*.98)||!(s.concurrency.observerPeak>=Math.ceil(target/20))||!(s.concurrency.observerMean>=Math.ceil(target/20)*.98)))failures.push(app.key+': observer target/exposure');
    for(const field of ['wireErrors','disconnects','subscriptionErrors','setupFailures','operationFailures','skipped'])if(s[field])failures.push(app.key+': '+field);
    const transport=s.diagnosticWireConnections?.counts;if(transport&&(transport.unexpectedCloses||transport.errors))failures.push(app.key+': transport failure');
   for(const name of ['read','write','realtime']){const m=s.metrics[name];if(m.failed)failures.push(app.key+': '+name+' failed');const invalid=limits.criterion==='observed-maximum'?(!Number.isFinite(m.maxMs)||m.maxMs<0||!Number.isFinite(limits[name+'MaxMs'])||m.maxMs>limits[name+'MaxMs']):(!Number.isFinite(m.p95Ms)||!Number.isFinite(m.p99Ms)||m.p95Ms>limits[name+'P95Ms']||m.p99Ms>limits[name+'P99Ms']);if(m.count&&invalid)failures.push(app.key+': '+name+' latency');}
   if(limits.criterion==='observed-maximum')for(const name of ['read','write']){const samples=s.canceledLatencySamplesMs?.[name];if(!Array.isArray(samples)||samples.length!==s.metrics[name].canceled||samples.some(ms=>!Number.isFinite(ms)||ms<0||ms>limits[name+'MaxMs']))failures.push(app.key+': '+name+' cancellation latency/accounting');}
   for(const[kind,interval]of [['read',app.readMs],['write',app.writeMs]]){const m=s.metrics[kind],expected=target*phase.holdSeconds/(interval/1000)*(phase.rateMultiplier??1),minimum=expected<30?Math.max(0,expected-3*Math.sqrt(expected)):Math.max(expected*.75,expected-3*Math.sqrt(expected));if(m.count<Math.floor(minimum))failures.push(app.key+': insufficient network '+kind+'s');if(!Number.isInteger(m.offered)||m.offered!==m.count+m.failed+m.canceled)failures.push(app.key+': incomplete '+kind+' accounting');}
   if(s.metrics.read.canceled+s.metrics.write.canceled>(s.phaseChurn||0))failures.push(app.key+': cancellations exceed intentional departures');
 }
 return failures;
}
