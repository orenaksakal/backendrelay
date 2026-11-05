import test from 'node:test';
import assert from 'node:assert/strict';
import {rowId,summary,verdict,profile} from './model.mjs';
test('synthetic rows are deterministic, isolated and valid UUIDs',()=>{assert.equal(rowId('one',1),rowId('one',1));assert.notEqual(rowId('one',1),rowId('two',1));assert.match(rowId('one',1),/^[a-f0-9-]{36}$/);});
test('latency summaries include tails and empty data is not success',()=>{assert.equal(summary([1,2,3,100]).p99Ms,100);assert.equal(summary([]).p95Ms,null);const r={users:200,holdSeconds:120,connectedAtHold:200,metrics:{read:{count:400,p95Ms:50,p99Ms:100},write:{count:80,p95Ms:50,p99Ms:100},realtime:{count:80,p95Ms:50,p99Ms:100}}};assert(verdict(r).passed);r.metrics.read.count=0;assert(!verdict(r).passed);});
test('5k light-active means 83 reads/sec and 17 writes/sec, not 5k RPS',()=>{assert.equal(5000/(profile.readIntervalMs/1000),5000/60);assert.equal(5000/(profile.writeIntervalMs/1000),5000/300);});
