// Transport diagnostics only. Never retain socket URLs, close reasons, error
// messages, frames or identities. Expected local shutdowns remain counted apart.
const errorCodes=new Set(['ECONNRESET','ETIMEDOUT','ECONNREFUSED','ENOTFOUND','EPIPE','EPROTO']);
const elapsed=(now,then)=>then===null?null:Math.max(0,now-then);
export class WireConnections {
 constructor(){this.counts={opened:0,closed:0,localCloses:0,unexpectedCloses:0,errors:0,localHandshakeCancellations:0};this.events=[];}
 attach(socket,clock=()=>performance.now(),wall=()=>Date.now()){
  const born=clock();let received=null,sent=null,ping=null,pings=0,local=false,opened=false;
  const retain=(kind,extra)=>{const at=new Date(wall()).toISOString(),now=clock();this.events.push({kind,at,ageMs:elapsed(now,born),sinceReceiveMs:elapsed(now,received),sinceSendMs:elapsed(now,sent),sincePingMs:elapsed(now,ping),pings,...extra});if(this.events.length>32)this.events.shift();};
  socket.on('open',()=>{opened=true;this.counts.opened++;});
  socket.on('message',()=>{received=clock();});
  socket.on('ping',()=>{ping=received=clock();pings++;});
  socket.on('pong',()=>{received=clock();});
  socket.on('close',code=>{this.counts.closed++;this.counts[local?'localCloses':'unexpectedCloses']++;if(!local)retain('close',{code:Number.isInteger(code)&&code>=0&&code<=4999?code:null});});
  socket.on('error',error=>{
   // ws emits this exact error when the SDK explicitly closes its initial
   // unauthenticated socket while replacing it during signInWithToken. It is
   // not an established-session loss or a remote handshake rejection. Retain
   // the separate count; never ignore another error or a nonlocal cancellation.
   if(local&&!opened&&received===null&&sent===null&&error?.message==='WebSocket was closed before the connection was established'){
    this.counts.localHandshakeCancellations++;return;
   }
   // ws emits this fixed message after an HTTP upgrade rejection. Parse only
   // its three-digit status; never retain the message, headers or response body.
   const rejected=/^Unexpected server response: ([1-5]\d\d)$/.exec(String(error?.message||''));
   const headers=socket._req?.res?.headers;
   this.counts.errors++;retain('error',{code:errorCodes.has(error?.code)?error.code:'other',...(rejected?{httpStatus:Number(rejected[1]),cloudflareResponse:headers?.server==='cloudflare',cloudflareChallenge:headers?.['cf-mitigated']==='challenge'}:{})});
  });
  return {sending:()=>{sent=clock();},closing:()=>{local=true;}};
 }
 snapshot(){return {counts:{...this.counts},events:this.events.map(event=>({...event}))};}
}
