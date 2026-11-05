import {test} from 'node:test';
import assert from 'node:assert/strict';
import {remoteProfiles,remoteProfile,remoteScenario} from './remote-scale-model.mjs';

test('both P7.9 profiles hold 200 sessions and three phases',()=>{
 for(const[name,profile]of Object.entries(remoteProfiles)){
  const scenario=remoteScenario(profile);
  assert.equal(scenario.apps,profile.apps,name);
  assert.equal(scenario.usersPerApp,profile.usersPerApp,name);
  assert.equal(scenario.totalUsers,200,name);
  assert.equal(scenario.groupSize,20,name);
  assert.deepEqual(scenario.phases.map(phase=>phase.name),['steady','burst','recovery'],name);
  assert.deepEqual(scenario.phases.map(phase=>phase.holdSeconds),[300,60,90],name);
  assert.deepEqual(scenario.phases.map(phase=>phase.rateMultiplier),[1,2,1],name);
  assert.ok(scenario.phases.every(phase=>phase.targets.length===profile.apps&&phase.targets.every(target=>target===profile.usersPerApp)),name);
 }
});

test('the 20x10 profile pools complete fixture rooms and one observer per app',()=>{
 const scenario=remoteScenario(remoteProfile('many'));
 assert.equal(scenario.poolPerApp,20);
 assert.equal(scenario.observers,20);
 assert.equal(remoteScenario(remoteProfile('single')).poolPerApp,200);
 assert.equal(remoteScenario(remoteProfile('single')).observers,10);
});

test('unknown profiles and out-of-range shapes are rejected',()=>{
 assert.throws(()=>remoteProfile('nope'),/unknown profile/);
 for(const apps of [0,101,1.5])assert.throws(()=>remoteScenario({apps}),/apps/);
 for(const usersPerApp of [0,2001,10.5])assert.throws(()=>remoteScenario({usersPerApp}),/usersPerApp/);
 for(const[name,value]of [['steadySeconds',0],['burstSeconds',-1],['recoverySeconds',NaN],['burstMultiplier',0],['rampPerSecond',0]])assert.throws(()=>remoteScenario({[name]:value}),new RegExp(name));
});
