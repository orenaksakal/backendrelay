import {apps as templates,phaseFailures} from './dau-model.mjs';

// Local multi-app benchmark model: N apps x a fixed concurrent-user target per
// app, driven against a local origin with a single op-rate burst phase. Apps
// are derived from the five DAU templates so read/write cadence and write shape
// stay consistent with the production generator without its DAU calibration.
export function localApps(count){
 if(!Number.isInteger(count)||count<1||count>100)throw new Error('apps must be an integer between 1 and 100');
 return Array.from({length:count},(_,index)=>{const template=templates[index%templates.length],suffix=Math.floor(index/templates.length)+1;return {key:template.key+'-'+suffix,title:template.title,readMs:template.readMs,writeMs:template.writeMs,sharedWrite:template.sharedWrite,append:template.append,template:template.key,index};});
}

export function localScenario({apps=10,usersPerApp=1000,steadySeconds=300,burstSeconds=60,recoverySeconds=90,burstMultiplier=2,rampPerSecond=50}={}){
 if(!Number.isInteger(apps)||apps<1||apps>100)throw new Error('apps must be an integer between 1 and 100');
 if(!Number.isInteger(usersPerApp)||usersPerApp<=0||usersPerApp>2000||usersPerApp%20!==0)throw new Error('usersPerApp must be a positive multiple of 20 no greater than 2000');
 for(const[name,value]of [['steadySeconds',steadySeconds],['burstSeconds',burstSeconds],['recoverySeconds',recoverySeconds]])if(!Number.isFinite(value)||value<=0)throw new Error(name+' must be finite and greater than 0');
 if(!Number.isFinite(burstMultiplier)||burstMultiplier<=0)throw new Error('burstMultiplier must be finite and greater than 0');
 if(!Number.isFinite(rampPerSecond)||rampPerSecond<=0)throw new Error('rampPerSecond must be finite and greater than 0');
 const totalUsers=apps*usersPerApp,groupSize=20,observers=totalUsers/groupSize;
 const shortRampSeconds=15,steadyRampSeconds=Math.max(30,Math.ceil(totalUsers/rampPerSecond));
 const phase=(name,holdSeconds,rampSeconds,rateMultiplier,kind)=>({name,targets:Array(apps).fill(usersPerApp),holdSeconds,rampSeconds,rateMultiplier,kind,requireObserverExposure:true});
 const phases=[phase('steady',steadySeconds,steadyRampSeconds,1,'steady'),phase('burst',burstSeconds,shortRampSeconds,burstMultiplier,'burst'),phase('recovery',recoverySeconds,shortRampSeconds,1,'recovery')];
 return {workloadModel:'local-multiapp-v1',apps,usersPerApp,totalUsers,observers,groupSize,poolPerApp:usersPerApp,phases,
  assumptions:{concurrency:'usersPerApp is the simultaneous session target per app (not DAU); every phase holds apps*usersPerApp total sessions',
   observers:'one additional observer session per 20-user group (index%20===0), giving totalUsers/20 observers',
   rateMultiplier:'burst divides each app read/write interval by burstMultiplier (default 2x op rate); steady and recovery run at 1x',
   ramp:'steady ramps from zero at most rampPerSecond sessions/s project-wide (min 30s); burst and recovery reuse steady targets so settle over a short 15s ramp',
   pool:'poolPerApp equals usersPerApp (already a multiple of 20); targets never exceed it, so no spike headroom is reserved'}};
}

export function localPhaseFailures(phase,statuses,limits,appList=localApps(phase.targets.length)){
 return phaseFailures(phase,statuses,limits,appList);
}
