import test from 'node:test';
import assert from 'node:assert/strict';
import {once} from 'node:events';
import {Worker} from 'node:worker_threads';
import {generatorWorkerEnvironment} from './generator-worker-env.mjs';

test('workers receive only generator transport settings, never parent credentials',()=>{
 const source={NODE_ENV:'development',LOADTEST_DNS_RESOLVERS:'8.8.8.8,8.8.4.4',UV_THREADPOOL_SIZE:'16',LOADTEST_APPS_JSON:'sensitive-fixture',LOADTEST_ADMIN_TOKEN:'sensitive-token',SSH_AUTH_SOCK:'sensitive-agent',NODE_OPTIONS:'unrequested-option'};
 const result=generatorWorkerEnvironment(source);
 assert.deepEqual(result,{NODE_ENV:'production',LOADTEST_DNS_RESOLVERS:'8.8.8.8,8.8.4.4',UV_THREADPOOL_SIZE:'16'});
 result.LOADTEST_DNS_RESOLVERS='changed';assert.equal(source.LOADTEST_DNS_RESOLVERS,'8.8.8.8,8.8.4.4');
 assert.deepEqual(generatorWorkerEnvironment({LOADTEST_DNS_RESOLVERS:'',UV_THREADPOOL_SIZE:undefined}),{NODE_ENV:'production'});
});

test('actual worker installs dedicated DNS without inheriting parent secrets',async()=>{
 const worker=new Worker(`const {parentPort,workerData}=require('node:worker_threads');import(workerData.moduleURL).then(({originLookup})=>parentPort.postMessage({backend:originLookup.snapshot().backend,resolvers:originLookup.snapshot().resolverServers,hasAdmin:Object.hasOwn(process.env,'LOADTEST_ADMIN_TOKEN')}));`,{eval:true,env:generatorWorkerEnvironment({LOADTEST_DNS_RESOLVERS:'8.8.8.8,8.8.4.4',UV_THREADPOOL_SIZE:'16',LOADTEST_ADMIN_TOKEN:'must-not-forward'}),workerData:{moduleURL:new URL('./dns-cache.mjs',import.meta.url).href}});
 try{const [result]=await once(worker,'message');assert.deepEqual(result,{backend:'dedicated-cares',resolvers:['8.8.8.8','8.8.4.4'],hasAdmin:false});}finally{await worker.terminate();}
});
