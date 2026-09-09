import test from 'node:test';
import assert from 'node:assert/strict';
import { db, defaultGainSettings, GainHistory, parseDiagnostics, type Diagnostics } from '../src/calibration-data';
const sample = (sequence=1, generation=1): Diagnostics => ({type:'diagnostics',paused:false,track:{video_id:'test',title:'Test',artist:'Artist'},frame:{generation,sequence,position_millis:100,settings:{...defaultGainSettings},input_rms:0.1,input_peak:0.2,output_rms:0.2,output_peak:0.4,rolling_rms:0.15,window_seconds:1,audible_seconds:1,requested_gain:2,preview_gain:1.5,applied_gain:1.9,xdgain_db:0,reference_rms:null,limiter_gain:1,master_gain:1}});
test('diagnostics use amplitude dB and refuse malformed or nonfinite values',()=>{
  assert.equal(db(0.1),-20);assert.equal(db(0),-96);assert.ok(Math.abs(db(2)-6.0206)<0.001);
  assert.deepEqual(parseDiagnostics(sample()),sample());
  for(const invalid of [{},null,{...sample(),paused:'false'}, {...sample(),frame:{...sample().frame,output_rms:NaN}}, {...sample(),frame:{...sample().frame,settings:{...defaultGainSettings,ra_seconds:0}}}])assert.equal(parseDiagnostics(invalid),null);
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
