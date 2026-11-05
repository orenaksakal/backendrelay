// Worker environments are intentionally isolated from parent credentials.
// Forward only the explicitly selected generator transport configuration.
export function generatorWorkerEnvironment(source=process.env){
 const result={NODE_ENV:'production'};
 for(const name of ['LOADTEST_DNS_RESOLVERS','UV_THREADPOOL_SIZE']){
  if(typeof source[name]==='string'&&source[name])result[name]=source[name];
 }
 return result;
}
