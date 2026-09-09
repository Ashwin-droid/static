import { useEffect, useRef, useState } from 'react';
import { GainHistory, db, defaultGainSettings, gainDb, level, parseDiagnostics, type Diagnostics, type GainSettings, type Point } from './calibration-data';
import './calibration.css';

const colors = { input: '#d7a971', output: '#73d2af', average: '#a79bec', requested: '#93a9c7', preview: '#dbb26c', applied: '#73d2af', xd: '#ac9cec', limiter: '#e98786' };
type Series = { name: string; color: string; value: (p: Point) => number; dashed?: boolean };
const loudness: Series[] = [
  { name: 'Song input', color: colors.input, value: p => db(p.frame.input_rms) },
  { name: 'Rolling average', color: colors.average, value: p => p.frame.settings.enabled && p.frame.rolling_rms > 0 ? db(p.frame.rolling_rms) : Number.NaN, dashed: true },
  { name: 'Processed output', color: colors.output, value: p => db(p.frame.output_rms) },
];
const gains: Series[] = [
  { name: 'Requested', color: colors.requested, value: p => db(p.frame.requested_gain), dashed:true },
  { name: 'Lookahead', color: colors.preview, value: p => db(p.frame.preview_gain), dashed:true },
  { name: 'Applied dGain', color: colors.applied, value: p => db(p.frame.applied_gain) },
  { name: 'xdGain', color: colors.xd, value: p => p.frame.xdgain_db },
  { name: 'Limiter', color: colors.limiter, value: p => db(p.frame.limiter_gain) },
];
function Graph({ points, series, min, max, band = false, title }: { points: Point[]; series: Series[]; min:number; max:number; band?:boolean; title:string }) {
  const canvas = useRef<HTMLCanvasElement>(null);
  const [width, setWidth] = useState(700);
  const [hover, setHover] = useState<number | null>(null);
  const height = band ? 136 : 96;
  useEffect(() => {
    const element = canvas.current!;
    const observer = new ResizeObserver(() => setWidth(Math.max(200, element.clientWidth)));
    observer.observe(element); return () => observer.disconnect();
  }, []);
  useEffect(() => {
    const element = canvas.current!; const ratio = Math.min(devicePixelRatio || 1, 2);
    element.width = width*ratio; element.height = height*ratio;
    const ctx = element.getContext('2d'); if (!ctx) return;
    ctx.scale(ratio,ratio); const left=42, right=width-12, top=8, bottom=height-24;
    const end = points.at(-1)?.time ?? 0, start=end-60_000;
    const x = (t:number) => left+(t-start)/60_000*(right-left);
    const y = (v:number) => bottom-(Math.max(min,Math.min(max,v))-min)/(max-min)*(bottom-top);
    ctx.font='10px ui-monospace, monospace'; ctx.lineWidth=1;
    for (let v=min;v<=max;v+=band?12:12) {
      ctx.strokeStyle=v===0?'#606577':'#292d37';ctx.beginPath();ctx.moveTo(left,y(v));ctx.lineTo(right,y(v));ctx.stroke();
      ctx.fillStyle='#929baa';ctx.fillText(String(v),4,y(v)+3);
    }
    for(let seconds=0;seconds<=60;seconds+=15){ const px=x(end-seconds*1000); ctx.fillStyle='#929baa'; ctx.fillText(seconds===0?'now':`−${seconds}s`,px-(seconds===0?18:8),height-5); }
    if(band) {
      ctx.fillStyle='#a79bec18';
      for(let i=1;i<points.length;i++) {
        const a=points[i-1],b=points[i]; if(b.time-a.time>1500 || !b.frame.settings.enabled || b.frame.rolling_rms===0)continue;
        const tolerance=20*Math.log10(1+b.frame.settings.flatten_percent/100);
        ctx.fillRect(x(a.time),y(db(b.frame.rolling_rms)+tolerance),x(b.time)-x(a.time)+1,y(db(b.frame.rolling_rms)-tolerance)-y(db(b.frame.rolling_rms)+tolerance));
      }
    }
    for(const line of series){
      ctx.strokeStyle=line.color;ctx.lineWidth=1.6;ctx.setLineDash(line.dashed?[4,3]:[]);ctx.beginPath();
      let drawing=false;points.forEach((point,i)=>{ const value=line.value(point);if(!Number.isFinite(value)){drawing=false;return;} const px=x(point.time),py=y(value); if(!drawing||i===0||point.time-points[i-1].time>1500)ctx.moveTo(px,py);else ctx.lineTo(px,py);drawing=true; });ctx.stroke();
    }
    ctx.setLineDash([]);
    if(hover!==null&&points[hover]){ctx.strokeStyle='#ffffff55';ctx.beginPath();ctx.moveTo(x(points[hover].time),top);ctx.lineTo(x(points[hover].time),bottom);ctx.stroke();}
  }, [points,width,hover,series,min,max,band,height]);
  const selected = hover === null ? null : points[hover];
  return <section className="gain-graph" aria-label={title}>
    <div className="gain-graph-heading"><h2>{title}</h2><span>{band?'dBFS · shaded area = allowed deviation':'dB · 0 = no change'}</span></div>
    <canvas ref={canvas} style={{height}} role="img" aria-label={`${title}. ${points.length} measurements in the last minute. Current values are shown below.`}
      onPointerLeave={()=>setHover(null)} onPointerMove={event=>{ const time=(event.nativeEvent.offsetX-42)/(width-54)*60_000+(points.at(-1)?.time??0)-60_000; let nearest=0;points.forEach((point,i)=>{if(Math.abs(point.time-time)<Math.abs((points[nearest]?.time??0)-time))nearest=i;});setHover(points.length?nearest:null); }} />
    <div className="gain-legend">{series.map(line=><span key={line.name}><i style={{background:line.color}} />{line.name}{selected&&<b>{Number.isFinite(line.value(selected))?line.value(selected).toFixed(1):'—'}</b>}</span>)}</div>
  </section>;
}

export function Calibration() {
  const [data,setData]=useState<Diagnostics|null>(null);
  const [status,setStatus]=useState('Connecting to Static…');
  const [error,setError]=useState<string|null>(null);
  const [draft,setDraft]=useState<GainSettings>(defaultGainSettings);
  const dirty=useRef(false), paused=useRef(false);
  const [frozen,setFrozen]=useState(false),[points,setPoints]=useState<Point[]>([]),[saving,setSaving]=useState(false);
  const fresh=useRef({generation:-1,sequence:-1,time:0});
  const history=useRef(new GainHistory()), socket=useRef<WebSocket|null>(null), saveTimer=useRef<ReturnType<typeof setTimeout>>();
  useEffect(()=>{
    document.title='Static · dGain calibration';
    let disposed=false, retry:ReturnType<typeof setTimeout>|undefined;
    const token=new URLSearchParams(location.hash.slice(1)).get('token');
    if(!token){setStatus('Open with static settings dGain calibrate');return;}
    const connect=()=>{
      const ws=new WebSocket(`wss://${location.host}/gain/ws?token=${encodeURIComponent(token)}`);socket.current=ws;
      ws.onopen=()=>{history.current.clear();setPoints([]);setStatus('Connected');};
      ws.onmessage=event=>{
        if(typeof event.data!=='string'||event.data.length>64_000)return;
        try{
          const message=JSON.parse(event.data);
          if(message.type==='saved'){dirty.current=false;setSaving(false);clearTimeout(saveTimer.current);setError(null);return;}
          if(message.type==='error'){setError(String(message.message));setSaving(false);clearTimeout(saveTimer.current);return;}
          if(message.type==='unavailable'){setStatus('Waiting for playback daemon');history.current.clear();return;}
          const next=parseDiagnostics(message);if(!next)return;
          if(next.frame.generation!==fresh.current.generation || next.frame.sequence!==fresh.current.sequence)fresh.current={generation:next.frame.generation,sequence:next.frame.sequence,time:performance.now()};
          setData(next);setStatus('Connected');
          if(!dirty.current)setDraft(next.frame.settings);
          if(!paused.current && history.current.append(next,performance.now()))setPoints([...history.current.points]);
        }catch{setError('Static sent an invalid diagnostics message.');}
      };
      ws.onerror=()=>ws.close();
      ws.onclose=()=>{setStatus('Disconnected · retrying');setSaving(false);clearTimeout(saveTimer.current);if(!disposed)retry=setTimeout(connect,1500);};
    };connect();
    return()=>{disposed=true;clearTimeout(retry);clearTimeout(saveTimer.current);if(socket.current){socket.current.onclose=null;socket.current.close();socket.current=null;}};
  },[]);
  const f=data?.frame, ready=status==='Connected';
  const editing=<K extends keyof GainSettings>(key:K,value:GainSettings[K])=>{dirty.current=true;setDraft(old=>({...old,[key]:value,...(key==='enabled'&&!value?{xdgain_enabled:false}:{})}));};
  const apply=()=>{
    if(socket.current?.readyState!==WebSocket.OPEN||saving)return;
    setSaving(true);setError(null);socket.current.send(JSON.stringify({settings:draft}));
    saveTimer.current=setTimeout(()=>{setSaving(false);setError('No settings confirmation yet. Check the connection before retrying.');},15_000);
  };
  const state = !ready?status:!data?.track?'No song selected':data.paused?'Playback paused':!f?.sequence?'Waiting for audio':performance.now()-fresh.current.time>1500?'Waiting for fresh audio':!f.settings.enabled?'dGain is off':f.input_rms<0.0001?'Silence · gain held':f.window_seconds<f.settings.ra_seconds?'Rolling average warming up':'Live';
  return <main className="gain-app">
    <header className="gain-header"><div><p className="gain-eyebrow">STATIC / PLAYBACK</p><h1>dGain calibration</h1></div><span className={`gain-state ${ready?'is-live':''}`}>{state}</span></header>
    <div className="gain-track"><strong>{data?.track?.title??'Play a song in Static to start measuring'}</strong><span>{data?.track?.artist}</span></div>
    <section className="gain-meters" aria-label="Current measurements">
      <div><span>Song input</span><strong>{f?.sequence?level(f.input_rms):'—'} <small>dBFS</small></strong></div>
      <div><span>Rolling average</span><strong>{f?.settings.enabled&&f.sequence?level(f.rolling_rms):'—'} <small>dBFS</small></strong></div>
      <div><span>dGain response</span><strong>{f?.sequence?gainDb(f.applied_gain):'—'} <small>dB</small></strong></div>
      <div><span>Processed output</span><strong>{f?.sequence?level(f.output_rms):'—'} <small>dBFS</small></strong></div>
    </section>
    <Graph points={points} series={loudness} min={-72} max={12} band title="Loudness" />
    <Graph points={points} series={gains} min={-48} max={24} title="Gain response" />
    <div className="gain-details">
      <span>Output peak <b>{f?.sequence?level(f.output_peak):'—'} dBFS</b></span>
      <span className={f&&f.limiter_gain<0.99?'gain-limiting':''}>Limiter <b>{f?.sequence?gainDb(f.limiter_gain):'—'} dB</b></span>
      <span>Master <b>{f?Math.round(f.master_gain*100):'—'}%</b></span>
      <span>xdGain <b>{f?.sequence?`${f.xdgain_db>=0?'+':''}${f.xdgain_db.toFixed(1)}`:'—'} dB</b></span>
      <span>xdGain reference <b>{level(f?.reference_rms)} dBFS</b></span>
      <span>Window <b>{f?.window_seconds.toFixed(1)??'0'} / {f?.settings.ra_seconds??15}s</b></span>
      <span>Audible audio <b>{f?.audible_seconds.toFixed(1)??'0'}s</b></span>
      <span>Lookahead <b>{f?.settings.enabled?'250':'0'} ms</b></span>
      <span>Attack / release <b>35 ms / {Math.round(250+(f?.settings.change_lazyness_percent??50)*17.5)} ms</b></span>
      <span>Change deadband <b>{((f?.settings.change_lazyness_percent??50)*0.03).toFixed(1)} dB</b></span>
    </div>
    <div className="gain-history-controls"><span>Last 60 seconds · hover graphs to inspect · levels are before the device mixer</span>
      <button onClick={()=>{paused.current=!frozen;setFrozen(!frozen);}}>{frozen?'Resume graph':'Freeze graph'}</button>
      <button onClick={()=>{history.current.clear();setPoints([]);}}>Clear graph</button>
    </div>
    <form className="gain-settings" onSubmit={event=>{event.preventDefault();apply();}}>
      <div className="gain-toggles"><label><input type="checkbox" checked={draft.enabled} onChange={event=>editing('enabled',event.target.checked)} />dGain</label><label><input type="checkbox" checked={draft.xdgain_enabled} disabled={!draft.enabled} onChange={event=>editing('xdgain_enabled',event.target.checked)} />xdGain across songs</label></div>
      <div className="gain-sliders">
        <label>Flatten tolerance <output>{draft.flatten_percent}%</output><input aria-label="Flatten tolerance" type="range" min="0" max="100" step="1" value={draft.flatten_percent} onChange={event=>editing('flatten_percent',Number(event.target.value))} /></label>
        <label>Rolling average <div><input aria-label="Rolling average seconds" type="number" min="0.1" max="120" step="0.1" value={Number.isFinite(draft.ra_seconds)?draft.ra_seconds:''} onChange={event=>editing('ra_seconds',event.target.valueAsNumber)} /><span> seconds</span></div></label>
        <label>Change lazyness <output>{draft.change_lazyness_percent}%</output><input aria-label="Change lazyness" type="range" min="0" max="100" step="1" value={draft.change_lazyness_percent} onChange={event=>editing('change_lazyness_percent',Number(event.target.value))} /></label>
      </div>
      <footer><p>Apply saves these playback settings. The rolling average refills after a change.</p><button type="submit" disabled={!ready||saving||!Number.isFinite(draft.ra_seconds)}>{saving?'Applying…':'Apply settings'}</button></footer>
    </form>
    {error&&<p className="gain-error" role="alert">{error}</p>}
  </main>;
}
