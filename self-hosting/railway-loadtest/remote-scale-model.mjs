import {phaseFailures} from './dau-model.mjs';

// Remote capacity-baseline profiles from P7.9. Both hold 200 concurrent
// sessions: one app carrying all 200, and twenty small apps with ten active
// users each. Kept here so the runner, its tests and the operator runbook share
// one definition.
export const remoteProfiles=Object.freeze({
 single:Object.freeze({apps:1,usersPerApp:200}),
 many:Object.freeze({apps:20,usersPerApp:10}),
});

export function remoteProfile(name){
 const profile=remoteProfiles[name];
 if(!profile)throw new Error(`unknown profile ${name}; use ${Object.keys(remoteProfiles).join(' or ')}`);
 return profile;
}

// The local benchmark requires usersPerApp to be a multiple of the 20-user
// fixture room. The 20x10 profile is deliberately smaller per app, so the
// identity pool is rounded up to a complete room while only usersPerApp
// sessions are active. Everything else matches localScenario.
export function remoteScenario({apps=1,usersPerApp=200,steadySeconds=300,burstSeconds=60,recoverySeconds=90,burstMultiplier=2,rampPerSecond=50}={}){
 if(!Number.isInteger(apps)||apps<1||apps>100)throw new Error('apps must be an integer between 1 and 100');
 if(!Number.isInteger(usersPerApp)||usersPerApp<1||usersPerApp>2000)throw new Error('usersPerApp must be an integer between 1 and 2000');
 for(const[name,value]of [['steadySeconds',steadySeconds],['burstSeconds',burstSeconds],['recoverySeconds',recoverySeconds]])if(!Number.isFinite(value)||value<=0)throw new Error(name+' must be finite and greater than 0');
 if(!Number.isFinite(burstMultiplier)||burstMultiplier<=0)throw new Error('burstMultiplier must be finite and greater than 0');
 if(!Number.isFinite(rampPerSecond)||rampPerSecond<=0)throw new Error('rampPerSecond must be finite and greater than 0');
 const groupSize=20,totalUsers=apps*usersPerApp,observers=apps*Math.ceil(usersPerApp/groupSize),poolPerApp=Math.ceil(usersPerApp/groupSize)*groupSize;
 const shortRampSeconds=15,steadyRampSeconds=Math.max(30,Math.ceil(totalUsers/rampPerSecond));
 const phase=(name,holdSeconds,rampSeconds,rateMultiplier,kind)=>({name,targets:Array(apps).fill(usersPerApp),holdSeconds,rampSeconds,rateMultiplier,kind,requireObserverExposure:true});
 const phases=[phase('steady',steadySeconds,steadyRampSeconds,1,'steady'),phase('burst',burstSeconds,shortRampSeconds,burstMultiplier,'burst'),phase('recovery',recoverySeconds,shortRampSeconds,1,'recovery')];
 return {workloadModel:'remote-multiapp-v1',apps,usersPerApp,totalUsers,observers,groupSize,poolPerApp,phases,
  assumptions:{concurrency:'usersPerApp is the simultaneous session target per app (not DAU); every phase holds apps*usersPerApp total sessions',
   observers:'one additional observer session per group of 20 active positions, at least one per app',
   rateMultiplier:'burst divides each app read/write interval by burstMultiplier (default 2x op rate); steady and recovery run at 1x',
   ramp:'steady ramps from zero at most rampPerSecond sessions/s project-wide (min 30s); burst and recovery reuse steady targets so settle over a short 15s ramp',
   pool:'the identity pool is rounded up to complete 20-user fixture rooms per app; only usersPerApp sessions are active'}};
}

export function remotePhaseFailures(phase,statuses,limits,appList){
 return phaseFailures(phase,statuses,limits,appList);
}
