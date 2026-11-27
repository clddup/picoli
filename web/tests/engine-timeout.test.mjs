import test from 'node:test';
import assert from 'node:assert/strict';
import { compressFile } from '../engine-client.js';
const file = new File(['original'], 'test.png');
function harness() {
  return {
    stopped: false,
    terminate() { this.stopped = true; },
    postMessage() { this.onmessage({ data: { type: 'phase', phase: 'compressing' } }); },
    result() { this.onmessage({ data: { type: 'result', buffer: new Uint8Array([1,2]).buffer, milliseconds: 123 } }); },
  };
}

test('slow compression warns without terminating and may still return its real result', async () => {
  const worker = harness();
  let warn;
  const warned = new Promise(resolve => { warn = resolve; });
  const result = compressFile(file,50,{workerFactory:()=>worker,slowAfter:5,timeout:1000,onPhase:phase=>{if(phase==='long-running')warn();}});
  await warned;
  assert.equal(worker.stopped,false);
  worker.result();
  assert.equal((await result).buffer.byteLength,2);
  assert.equal(worker.stopped,true);
});

test('a final hard timeout still terminates a slow worker and ignores late success', async () => {
  const worker=harness();
  await assert.rejects(compressFile(file,50,{workerFactory:()=>worker,slowAfter:5,timeout:15}),/处理超时/);
  assert.equal(worker.stopped,true);
  worker.result();
});

test('manual cancellation still works after the slow warning', async () => {
  const worker=harness(), controller=new AbortController();
  let warn;
  const warned=new Promise(resolve=>{warn=resolve;});
  const result=compressFile(file,50,{signal:controller.signal,workerFactory:()=>worker,slowAfter:5,timeout:1000,onPhase:phase=>{if(phase==='long-running')warn();}});
  await warned;controller.abort();
  await assert.rejects(result,{name:'AbortError'});
  assert.equal(worker.stopped,true);
});

test('duplicate compression phase messages cannot indefinitely reset the hard timeout', async () => {
  const worker=harness();
  const interval=setInterval(()=>worker.onmessage?.({data:{type:'phase',phase:'compressing'}}),3);
  try {
    await assert.rejects(compressFile(file,50,{workerFactory:()=>worker,slowAfter:5,timeout:20}),/处理超时/);
    assert.equal(worker.stopped,true);
  } finally { clearInterval(interval); }
});
