import test from 'node:test';
import assert from 'node:assert/strict';
import {WireTimings} from './wire-timing.mjs';
test('wire timing distinguishes protocol round trips without storing IDs or frames',()=>{
 const timings=new WireTimings();timings.record('write',1792429050000,3446);timings.record('read',1792429050100,3230);timings.record('read',1792429050101,12);
 assert.deepEqual(timings.snapshot(),{counts:{read:2,write:1},maxMs:{read:3230,write:3446},slowCompletions:2,samples:[{operation:'write',sentAt:'2026-10-19T16:57:30.000Z',elapsedMs:3446},{operation:'read',sentAt:'2026-10-19T16:57:30.100Z',elapsedMs:3230}]});
 const copy=timings.snapshot();copy.samples[0].operation='private';assert.equal(timings.snapshot().samples[0].operation,'write');
});
test('wire diagnostics are bounded and ignore arbitrary categories and invalid clocks',()=>{
 const timings=new WireTimings();for(let n=0;n<100;n++)timings.record('read',1792429050000+n,2000+n);
 for(const args of [['token-sensitive',1,1],['read',NaN,1],['read',1,-1],['read',1,Infinity]])timings.record(...args);
 const d=timings.snapshot();assert.equal(d.counts.read,100);assert.equal(d.samples.length,32);assert.equal(d.samples[0].elapsedMs,2099);assert.equal(d.slowCompletions,100);assert(!JSON.stringify(d).includes('sensitive'));
});
