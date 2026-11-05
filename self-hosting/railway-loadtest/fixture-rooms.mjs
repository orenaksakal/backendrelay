// Registered room membership is independent of how many members are active.
// Partial last rooms must not redefine membership when a later tier shrinks.
export function fixtureIdentityCapacity(active,groupSize=20){
 if(!Number.isInteger(active)||active<1||!Number.isInteger(groupSize)||groupSize<1)throw new Error('Invalid fixture capacity');
 return Math.ceil(active/groupSize)*groupSize;
}
export function fixtureRoomMembers(identities,index,groupSize=20){
 const start=Math.floor(index/groupSize)*groupSize,group=identities.slice(start,start+groupSize);
 if(group.length!==groupSize)throw new Error('Incomplete canonical fixture room');
 return group.map(identity=>identity.userId);
}
