import React from 'react';
import ReactDOM from 'react-dom/client';
const App = React.lazy(() => import('./App').then(module => ({ default: module.App })));
const Calibration = React.lazy(() => import('./Calibration').then(module => ({ default: module.Calibration })));
import './style.css';

ReactDOM.createRoot(document.getElementById('root')!).render(<React.StrictMode><React.Suspense fallback={<p style={{padding:24}}>Opening Static…</p>}>{location.pathname === '/calibrate' ? <Calibration /> : <App />}</React.Suspense></React.StrictMode>);
