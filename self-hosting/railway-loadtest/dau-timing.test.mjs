import test from 'node:test';
import assert from 'node:assert/strict';
import {timingDiagnostics,recordTiming,copyTiming} from './dau-timing.mjs';
const start=Date.parse('2026-10-19T06:00:00Z');
test('tail timestamps do not change raw latencies or record fast/boundary completions',()=>{
 const timing=timingDiagnostics();
 for(const elapsedMs of [0,100,2000])recordTiming(timing,{kind:'read',startedAtMs:start,elapsedMs});
 assert.equal(timing.slowCompletions,0);
 recordTiming(timing,{kind:'read',startedAtMs:start,elapsedMs:2000.1});
 assert.deepEqual(timing.samples,[{kind:'read',startedAt:'2026-10-19T06:00:00.000Z',elapsedMs:2000.1}]);
});
test('write diagnostics preserve acknowledgment and remote times without payloads or identities',()=>{
 const timing=timingDiagnostics();
 recordTiming(timing,{kind:'write',startedAtMs:start,elapsedMs:4744,ackMs:500,realtimeMs:4743,shared:true,token:'must-not-retain',query:{payload:'must-not-retain'},userId:'must-not-retain'});
 assert.deepEqual(timing.samples,[{kind:'write',startedAt:'2026-10-19T06:00:00.000Z',elapsedMs:4744,ackMs:500,realtimeMs:4743,shared:true}]);
});
test('bounded diagnostic records keep the worst tails while counting every slow completion',()=>{
 const timing=timingDiagnostics();
 for(let n=100;n>0;n--)recordTiming(timing,{kind:'read',startedAtMs:start+n,elapsedMs:2000+n});
 assert.equal(timing.slowCompletions,100);
 assert.equal(timing.samples.length,32);
 assert.equal(timing.samples[0].elapsedMs,2100);
 assert.equal(timing.samples.at(-1).elapsedMs,2069);
 const copy=copyTiming(timing);copy.samples[0].elapsedMs=0;copy.samples.pop();
 assert.equal(timing.samples[0].elapsedMs,2100);assert.equal(timing.samples.length,32);
});
test('local-only writes retain a null remote observation rather than inventing latency',()=>{
 const timing=timingDiagnostics();
 recordTiming(timing,{kind:'write',startedAtMs:start,elapsedMs:2100,ackMs:2099,realtimeMs:null,shared:false});
 assert.equal(timing.samples[0].realtimeMs,null);
});
test('diagnostic timing rejects nonfinite durations, invalid timestamps and arbitrary operation kinds',()=>{
 for(const sample of [{kind:'payload',startedAtMs:start,elapsedMs:3001},{kind:'read',startedAtMs:Infinity,elapsedMs:3001},{kind:'read',startedAtMs:8.65e15,elapsedMs:3001},{kind:'read',startedAtMs:start,elapsedMs:NaN},{kind:'write',startedAtMs:start,elapsedMs:3001,ackMs:1,realtimeMs:Infinity,shared:true}])assert.throws(()=>recordTiming(timingDiagnostics(),sample));
});
