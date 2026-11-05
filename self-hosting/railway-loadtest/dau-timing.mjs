// Optional tail diagnostics only. Raw latency arrays and acceptance are separate.
// Keep timestamps/durations and fixed operation kinds, never identities/payloads.
const thresholdMs=2000,maximumRecords=32;
export function timingDiagnostics(){return {thresholdMs,maximumRecords,slowCompletions:0,samples:[]};}
export function recordTiming(timing,{kind,startedAtMs,elapsedMs,ackMs,realtimeMs,shared}){
 if(!['read','write'].includes(kind)||!Number.isFinite(startedAtMs)||Math.abs(startedAtMs)>8.64e15||!Number.isFinite(elapsedMs)||elapsedMs<0)throw new Error('invalid diagnostic timing');
 const sample={kind,startedAt:new Date(startedAtMs).toISOString(),elapsedMs};
 if(kind==='write'){
  if(!Number.isFinite(ackMs)||ackMs<0||!(realtimeMs===null||Number.isFinite(realtimeMs)&&realtimeMs>=0)||typeof shared!=='boolean')throw new Error('invalid write diagnostic timing');
  Object.assign(sample,{ackMs,realtimeMs,shared});
 }
 const severity=s=>Math.max(s.elapsedMs,s.ackMs||0,s.realtimeMs||0);
 if(severity(sample)<=thresholdMs)return;
 timing.slowCompletions++;
 timing.samples.push(sample);
 timing.samples.sort((a,b)=>severity(b)-severity(a));
 timing.samples.length=Math.min(timing.samples.length,maximumRecords);
}
export function copyTiming(timing){return {...timing,samples:timing.samples.map(sample=>({...sample}))};}
