// Fixed-stage connection diagnostics only; never retain addresses, URLs or errors.
export class WireHandshakes {
 constructor(){this.pending=new Set();this.completed={opened:0,closedBeforeOpen:0};this.maximumMs={lookup:0,connect:0,secureConnect:0,open:0};this.slow=[];}
 attach(ws,clock=()=>performance.now(),wall=()=>Date.now()){
  const state={stage:'socket',born:clock(),timings:{}};this.pending.add(state);let done=false;
  const finish=opened=>{
   if(done)return;done=true;this.pending.delete(state);
   this.completed[opened?'opened':'closedBeforeOpen']++;
   const elapsedMs=clock()-state.born;
   if(elapsedMs>=2000){this.slow.push({at:new Date(wall()).toISOString(),outcome:opened?'opened':'closed-before-open',stage:state.stage,elapsedMs,timings:{...state.timings}});if(this.slow.length>32)this.slow.shift();}
  };
  const measure=(event,stage)=>{const ms=clock()-state.born;state.timings[event]=ms;this.maximumMs[event]=Math.max(this.maximumMs[event],ms);state.stage=stage;};
  ws._req?.on('socket',socket=>{
   state.stage='dns';
   socket.once('lookup',()=>measure('lookup','connect'));
   socket.once('connect',()=>measure('connect','tls'));
   socket.once('secureConnect',()=>measure('secureConnect','upgrade'));
  });
  ws.on('open',()=>{measure('open','open');finish(true);});
  ws.on('close',()=>finish(false));
 }
 snapshot(){const pendingByStage={socket:0,dns:0,connect:0,tls:0,upgrade:0};for(const state of this.pending)if(Object.hasOwn(pendingByStage,state.stage))pendingByStage[state.stage]++;return {pendingByStage,completed:{...this.completed},maximumMs:{...this.maximumMs},slow:this.slow.map(s=>({...s,timings:{...s.timings}}))};}
}
