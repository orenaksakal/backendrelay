import {randomUUID} from 'node:crypto';
import {makeClient,deadline} from './session.mjs';
import {rowId,profile} from './model.mjs';
const privateFields=['id','ownerId','title','completed','revision','updatedAt'];
const sharedFields=['id','roomId','authorId','text','revision','updatedAt'];
export async function connect(app,identity,key,onDisconnect,history=[]){
 const db=makeClient(app.appId,key),state={db,identity,closed:false,ready:false,privateRows:[],sharedRows:[],pending:new Map(),subscriptionErrors:0};let phase='auth';
 const query={todos:{$:{where:{ownerId:identity.userId},limit:10,fields:privateFields}},messages:{$:{where:{roomId:identity.roomId},order:{updatedAt:'desc'},limit:20,fields:sharedFields}}};
 try{
  await deadline(db.auth.signInWithToken(identity.token));
  phase='subscribe';
  await deadline(new Promise((resolve,reject)=>{state.unsubscribe=db.subscribeQuery(query,result=>{
   if(result.error){state.subscriptionErrors++;const type=String(result.error.type||'unknown');reject(new Error('subscription-error-'+(/^[a-z-]{1,64}$/.test(type)?type:'unknown')));return;}
   if(result.data){
    state.privateRows=result.data.todos;state.sharedRows=result.data.messages;
     const isolation=state.privateRows.some(r=>r.ownerId!==identity.userId)?'private-isolation-error':state.sharedRows.some(r=>r.roomId!==identity.roomId)?'shared-room-isolation-error':state.sharedRows.some(r=>!identity.members.includes(r.authorId))?'shared-author-isolation-error':null;
     if(isolation){state.subscriptionErrors++;reject(new Error(isolation));return;}
    if(!state.ready){state.ready=true;resolve();}
    for(const row of [...state.privateRows,...state.sharedRows]){const pending=state.pending.get(row.revision);if(pending){pending.resolve(performance.now()-pending.at);state.pending.delete(row.revision);}}
   }
  });}));
  if(state.privateRows.length!==10||!state.sharedRows.length)throw new Error(`fixture-mismatch-private-${state.privateRows.length}-shared-${state.sharedRows.length}`);
  if(state.sharedRows.some(row=>'memberIds'in row))throw new Error('field-projection-failed');
  state.connection=db.subscribeConnectionStatus(status=>{if(state.ready&&!state.closed&&status!=='authenticated')onDisconnect();});
  state.read=async(shared=false,page=false)=>{
   const nonce=randomUUID();let q;
   if(shared){const last=state.sharedRows.at(-1);q={messages:{$:{where:{roomId:identity.roomId,revision:{$ne:nonce},...(page&&last?{updatedAt:{$lt:last.updatedAt}}:{})},order:{updatedAt:'desc'},limit:20,fields:sharedFields}}};}
   else q={todos:{$:{where:{ownerId:identity.userId,revision:{$ne:nonce}},limit:10,fields:privateFields}}};
   const result=await deadline(db.queryOnce(q));
   if(shared){if(result.data.messages.some(r=>r.roomId!==identity.roomId||!identity.members.includes(r.authorId)||'memberIds'in r))throw new Error('shared-read-isolation');}
   else if(result.data.todos.length!==10||result.data.todos.some(r=>r.ownerId!==identity.userId))throw new Error('private-read-isolation');
  };
  state.write=async(shared=false)=>{
   const revision=randomUUID(),begin=performance.now(),remote=state.observer;let resolve;const observed=new Promise(r=>{resolve=r;});remote?.pending.set(revision,{at:begin,resolve});
   try{
    let steps;
    if(shared){const id=app.append?randomUUID():rowId(identity.userId,100);const data={revision,text:`Synthetic ${app.key} update ${revision.slice(0,8)}`,updatedAt:Date.now()};
     if(app.append)Object.assign(data,{roomId:identity.roomId,authorId:identity.userId,memberIds:identity.members});
     steps=[db.tx.messages[id].update(data)];if(app.append){history.push(id);if(history.length>30)steps.push(db.tx.messages[history.shift()].delete());}
    }else steps=[db.tx.todos[rowId(identity.userId,0)].update({revision,title:'Synthetic task '+revision.slice(0,8),completed:Math.random()<.5,updatedAt:Date.now()})];
    const ack=await deadline(db.transact(steps));if(ack.status!=='synced')throw new Error('write-not-synced');
     const ackMs=performance.now()-begin,realtimeMs=remote?await deadline(observed,Math.max(1,profile.operationTimeoutMs-ackMs)):null;return {ackMs,realtimeMs};
   }finally{remote?.pending.delete(revision);}
  };
   state.close=()=>{if(state.closed)return;state.closedAtMs=performance.now();state.closed=true;state.connection?.();state.unsubscribe?.();state.observer?.close();db.shutdown();};
  return state;
 }catch(error){state.closed=true;state.unsubscribe?.();db.shutdown();const safe=/^(operation-timeout|field-projection-failed|isolation-error|private-isolation-error|shared-room-isolation-error|shared-author-isolation-error|fixture-mismatch-private-\d+-shared-\d+|subscription-error-[a-z-]+)$/.test(error.message)?error.message:'failed';throw new Error('DAU-session-'+phase+'-'+safe);}
}
