import test from 'node:test';import assert from 'node:assert/strict';import {latencyAcceptance as limits} from './dau-model.mjs';import {localApps,localScenario,localPhaseFailures} from './local-scale-model.mjs';
const apps=localApps(10);
test('local apps duplicate the five templates round-robin with unique keys and preserved profiles',()=>{
 assert.equal(apps.length,10);assert.equal(new Set(apps.map(a=>a.key)).size,10);
 assert.deepEqual(apps.map(a=>a.key),['tasks-1','feed-1','analytics-1','chat-1','mobile-1','tasks-2','feed-2','analytics-2','chat-2','mobile-2']);
 assert.deepEqual(apps[0],{key:'tasks-1',title:'Tasks',readMs:30000,writeMs:120000,sharedWrite:.5,append:false,template:'tasks',index:0});
 assert.deepEqual(apps[5],{key:'tasks-2',title:'Tasks',readMs:30000,writeMs:120000,sharedWrite:.5,append:false,template:'tasks',index:5});
 assert.deepEqual(apps[3],{key:'chat-1',title:'Chat',readMs:20000,writeMs:30000,sharedWrite:1,append:true,template:'chat',index:3});
 for(const app of apps)assert.equal(app.template,['tasks','feed','analytics','chat','mobile'][app.index%5]);
 assert.throws(()=>localApps(0));assert.throws(()=>localApps(101));assert.throws(()=>localApps(1.5));assert.throws(()=>localApps('10'));
});
test('many small apps: 50 apps x 100 users keep template round-robin and observer coverage',()=>{
 const s=localScenario({apps:50,usersPerApp:100});assert.equal(s.totalUsers,5000);assert.equal(s.observers,250);
 const many=localApps(50);assert.equal(new Set(many.map(a=>a.key)).size,50);assert.equal(many[49].key,'mobile-10');
});
test('10 apps x 1000 users totals 10000 sessions and 500 observers',()=>{
 const s=localScenario();assert.equal(s.workloadModel,'local-multiapp-v1');assert.equal(s.apps,10);assert.equal(s.usersPerApp,1000);
 assert.equal(s.totalUsers,10000);assert.equal(s.observers,500);assert.equal(s.groupSize,20);assert.equal(s.poolPerApp,1000);
 assert.deepEqual(s.phases.map(p=>p.name),['steady','burst','recovery']);
 for(const p of s.phases){assert.deepEqual(p.targets,Array(10).fill(1000));assert.equal(p.requireObserverExposure,true);assert(p.holdSeconds>0);assert(p.rampSeconds>=15);}
 assert.equal(s.phases.find(p=>p.name==='steady').rateMultiplier,1);assert.equal(s.phases.find(p=>p.name==='recovery').rateMultiplier,1);
});
test('burst phase carries the rate multiplier and validation rejects bad inputs',()=>{
 const s=localScenario({burstMultiplier:3});assert.equal(s.phases.find(p=>p.name==='burst').rateMultiplier,3);assert.equal(s.phases.find(p=>p.name==='burst').kind,'burst');
 for(const bad of [0,-20,19,2001,1001,1000.5,'1000',NaN])assert.throws(()=>localScenario({usersPerApp:bad}),/usersPerApp/);
 for(const bad of [0,101,1.5,'10',NaN])assert.throws(()=>localScenario({apps:bad}),/apps/);
 assert.throws(()=>localScenario({burstMultiplier:0}),/burstMultiplier/);assert.throws(()=>localScenario({burstMultiplier:-1}),/burstMultiplier/);
});
const good=(phase)=>apps.map(app=>({app:app.key,concurrency:{samples:phase.holdSeconds,peak:1000,mean:1000,observerPeak:50,observerMean:50},canceledLatencySamplesMs:{read:[],write:[]},metrics:{read:{count:20000,offered:20000,canceled:0,failed:0,maxMs:500},write:{count:20000,offered:20000,canceled:0,failed:0,maxMs:500},realtime:{count:5000,offered:5000,canceled:0,failed:0,maxMs:500}}}));
test('verdict accepts a healthy 10-app burst status set',()=>{
 const burst=localScenario().phases.find(p=>p.name==='burst');assert.deepEqual(localPhaseFailures(burst,good(burst),limits,apps),[]);
});
test('verdict rejects a missing app, a 3001ms max, a wire error, and a 1x-passing count at 2x',()=>{
 const burst=localScenario().phases.find(p=>p.name==='burst');
 assert(localPhaseFailures(burst,good(burst).slice(1),limits,apps).includes('incomplete 10-app accounting'));
 const slow=good(burst);slow[0].metrics.read.maxMs=3001;assert(localPhaseFailures(burst,slow,limits,apps).includes('tasks-1: read latency'));
 const wired=good(burst);wired[1].wireErrors=1;assert(localPhaseFailures(burst,wired,limits,apps).includes('feed-1: wireErrors'));
 const sparse=good(burst);sparse[0].metrics.read.count=2500;sparse[0].metrics.read.offered=2500;
 assert.deepEqual(localPhaseFailures({...burst,rateMultiplier:1},sparse,limits,apps),[]);
 assert(localPhaseFailures(burst,sparse,limits,apps).includes('tasks-1: insufficient network reads'));
});
