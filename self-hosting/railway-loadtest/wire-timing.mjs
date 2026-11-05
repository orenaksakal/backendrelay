// Diagnostic wire round-trip clocks only. No frames, event IDs, payloads,
// tokens, query keys or user/app identifiers leave the in-memory socket map.
export class WireTimings {
 constructor(){this.counts={read:0,write:0};this.maxMs={read:null,write:null};this.slowCompletions=0;this.samples=[];}
 record(operation,sentAtMs,elapsedMs){
  if(!['read','write'].includes(operation)||!Number.isFinite(sentAtMs)||sentAtMs<=0||!Number.isFinite(elapsedMs)||elapsedMs<0)return;
  this.counts[operation]++;this.maxMs[operation]=Math.max(this.maxMs[operation]??0,elapsedMs);
  if(elapsedMs<2000)return;this.slowCompletions++;
  this.samples.push({operation,sentAt:new Date(sentAtMs).toISOString(),elapsedMs});this.samples.sort((a,b)=>b.elapsedMs-a.elapsedMs);this.samples.length=Math.min(32,this.samples.length);
 }
 snapshot(){return {counts:{...this.counts},maxMs:{...this.maxMs},slowCompletions:this.slowCompletions,samples:this.samples.map(s=>({...s}))};}
}
