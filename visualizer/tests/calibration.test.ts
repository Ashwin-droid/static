import test from 'node:test';
import assert from 'node:assert/strict';
import { db, defaultGainSettings, GainHistory, parseDiagnostics, type Diagnostics } from '../src/calibration-data';
const sample = (sequence=1, generation=1): Diagnostics => ({type:'diagnostics',paused:false,track:{video_id:'test',title:'Test',artist:'Artist'},frame:{generation,sequence,position_millis:100,opening_guard_millis:0,settings:{...defaultGainSettings},input_rms:0.1,input_peak:0.2,gain_output_rms:0.2,gain_output_peak:0.4,output_rms:0.1,output_peak:0.2,rolling_rms:0.15,window_seconds:1,audible_seconds:1,requested_gain:2,preview_gain:1.5,applied_gain:1.9,xdgain_reference_dbfs:null,limiter_gain:1,master_gain:1}});
test('diagnostics use amplitude dB and refuse malformed or nonfinite values',()=>{
  assert.equal(db(0.1),-20);assert.equal(db(0),-96);assert.ok(Math.abs(db(2)-6.0206)<0.001);
  assert.deepEqual(parseDiagnostics(sample()),sample());
  for(const invalid of [{},null,{...sample(),paused:'false'}, {...sample(),frame:{...sample().frame,gain_output_rms:undefined}}, {...sample(),frame:{...sample().frame,gain_output_rms:NaN}}, {...sample(),frame:{...sample().frame,output_rms:NaN}}, {...sample(),frame:{...sample().frame,settings:{...defaultGainSettings,ra_seconds:0}}}])assert.equal(parseDiagnostics(invalid),null);
});
test('history records fresh output only, remains bounded, and resets for new streams',()=>{
  const history=new GainHistory();
  assert.ok(history.append(sample(),0));assert.ok(!history.append(sample(),100));
  assert.ok(!history.append({...sample(2),paused:true},200));assert.ok(!history.append({...sample(2),track:null},300));
  for(let i=2;i<1000;i++)history.append(sample(i),i*100);
  assert.ok(history.points.length<=600);assert.ok(history.points[0].time>=39900);
  assert.ok(history.append(sample(1,2),100000));assert.equal(history.points.length,1);
  assert.ok(!history.append(sample(2000,1),100100));
  history.clear();assert.ok(history.append(sample(1,1),101000));
});

test('previous-song reference accepts negative dBFS and remains optional on the first song',()=>{
  const chained={...sample(),frame:{...sample().frame,xdgain_reference_dbfs:-22.5}};
  assert.equal(parseDiagnostics(chained)?.frame.xdgain_reference_dbfs,-22.5);
  assert.equal(parseDiagnostics(sample())?.frame.xdgain_reference_dbfs,null);
  assert.equal(parseDiagnostics({...chained,frame:{...chained.frame,xdgain_reference_dbfs:NaN}}),null);
});

test('WRA weight display varies smoothly and keeps xdGain at exactly twenty percent',async()=>{
  const { wraWeights }=await import('../src/calibration-data');
  for(const lazy of [0,12.5,25,50,75,99.9,100]){
    const settings={...defaultGainSettings,change_lazyness_percent:lazy,xdgain_enabled:true};
    const off=wraWeights(settings,false),on=wraWeights(settings,true);
    assert.ok(Math.abs(off.input+off.history-1)<1e-12);
    assert.equal(on.reference,.2);assert.ok(Math.abs(on.input+on.reference+on.history-1)<1e-12);
    assert.ok(Math.abs(on.input-.8*off.input)<1e-12);
  }
});
test('graph retains earlier samples when playback position seeks within one song',()=>{
  const history=new GainHistory();history.append(sample(1),0);
  const jumped=sample(2);jumped.frame.position_millis=60000;history.append(jumped,100);
  const back=sample(3);back.frame.position_millis=15000;history.append(back,200);
  assert.equal(history.points.length,3);
});
