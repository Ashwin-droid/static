use std::collections::VecDeque;
use std::sync::{
    Arc, Mutex, OnceLock,
    atomic::{AtomicBool, Ordering},
};

use gst::{glib, prelude::*, subclass::prelude::*};
use gstreamer as gst;

use super::band_isolation::BandIsolator;
use super::dynamic_gain::{DynamicGain, GainTarget};
use super::spectrum::Spectrum;

const LOOKAHEAD_MS: u64 = 250;

struct Pending {
    buffer: gst::Buffer,
    frames: usize,
    position_ns: u64,
    target: GainTarget,
}

#[derive(Default)]
struct BufferedAudio {
    blocks: VecDeque<Pending>,
    frames: usize,
    revision: u64,
    stream_revision: u64,
}

impl BufferedAudio {
    fn clear(&mut self, processor: &DynamicGain) {
        self.blocks.clear();
        self.frames = 0;
        self.revision = processor.revision();
        self.stream_revision = processor.stream_revision();
    }

    fn synchronize(&mut self, processor: &mut DynamicGain) -> Result<bool, gst::FlowError> {
        if self.revision == processor.revision() {
            return Ok(false);
        }
        if self.stream_revision != processor.stream_revision() {
            self.clear(processor);
        } else {
            // Settings invalidate old gain targets, not the song's PCM. Replay
            // original queued audio into the freshly reset analysis, retaining
            // every frame and timestamp even while a settings control repeats.
            for block in &mut self.blocks {
                block.target = processor.analyze_at(
                    block
                        .buffer
                        .map_readable()
                        .map_err(|_| gst::FlowError::Error)?
                        .as_slice(),
                    Some(block.position_ns),
                );
            }
            self.revision = processor.revision();
        }
        Ok(true)
    }
}

mod imp {
    use super::*;

    pub struct Lookahead {
        pub(super) processor: OnceLock<Arc<Mutex<DynamicGain>>>,
        pub(super) spectrum: OnceLock<Arc<Spectrum>>,
        pub(super) isolation: OnceLock<Arc<Mutex<BandIsolator>>>,
        pending: Mutex<BufferedAudio>,
        flushing: AtomicBool,
        sink: gst::Pad,
        src: gst::Pad,
    }

    impl Lookahead {
        fn reset(&self) {
            let mut pending = self.pending.lock().unwrap();
            if let Some(processor) = self.processor.get() {
                let mut processor = processor.lock().unwrap();
                processor.reset();
                pending.clear(&processor);
            }
            if let Some(spectrum) = self.spectrum.get() {
                spectrum.reset();
            }
            if let Some(isolation) = self.isolation.get() {
                isolation.lock().unwrap().reset_stream();
            }
        }

        fn emit(&self, drain: bool) -> Result<gst::FlowSuccess, gst::FlowError> {
            loop {
                if self.flushing.load(Ordering::Acquire) {
                    return Err(gst::FlowError::Flushing);
                }
                let output = {
                    let mut pending = self.pending.lock().unwrap();
                    let mut processor = self.processor.get().unwrap().lock().unwrap();
                    if pending.synchronize(&mut processor)? {
                        self.spectrum.get().unwrap().reset();
                    }
                    let (rate, channels) = processor.format();
                    let hold_frames = if processor.enabled() {
                        rate * LOOKAHEAD_MS as usize / 1000
                    } else {
                        0
                    };
                    let Some(front) = pending.blocks.front() else {
                        return Ok(gst::FlowSuccess::Ok);
                    };
                    if !drain && pending.frames - front.frames < hold_frames {
                        return Ok(gst::FlowSuccess::Ok);
                    }
                    // A short minimum hold plus slow recovery prevents individual
                    // waveform troughs from repeatedly increasing the gain.
                    let mut upcoming = front.target;
                    for block in &pending.blocks {
                        upcoming.dynamic = upcoming.dynamic.min(block.target.dynamic);
                    }
                    let mut block = pending.blocks.pop_front().unwrap();
                    pending.frames -= block.frames;
                    let position = block.buffer.pts().map(|pts| pts.mseconds());
                    {
                        let mut map = block
                            .buffer
                            .make_mut()
                            .map_writable()
                            .map_err(|_| gst::FlowError::Error)?;
                        processor.render(map.as_mut_slice(), block.target, upcoming);
                        processor.set_diagnostic_position(position);
                        // The visualiser sees the whole song; isolation only
                        // decides which of its bands reach the speakers.
                        self.spectrum
                            .get()
                            .unwrap()
                            .capture(map.as_slice(), rate as u32, channels);
                        if let Some(isolation) = self.isolation.get() {
                            isolation.lock().unwrap().process_bytes(
                                map.as_mut_slice(),
                                rate as u32,
                                channels,
                            );
                        }
                    }
                    block.buffer
                };
                // Pushing can block in preroll or the audio sink. Never hold the
                // DSP/settings mutex or queue mutex while waiting downstream.
                self.src.push(output)?;
            }
        }

        fn chain(&self, buffer: gst::Buffer) -> Result<gst::FlowSuccess, gst::FlowError> {
            if self.flushing.load(Ordering::Acquire) {
                return Err(gst::FlowError::Flushing);
            }
            if buffer.flags().contains(gst::BufferFlags::DISCONT) {
                self.reset();
            }
            let (rate, channels, fallback_position) = {
                let processor = self.processor.get().unwrap().lock().unwrap();
                let (rate, channels) = processor.format();
                (rate, channels, processor.analysis_position_ns())
            };
            let buffer_position = buffer.pts().map_or(fallback_position, |pts| pts.nseconds());
            let frame_bytes = channels * 4;
            if !buffer.size().is_multiple_of(frame_bytes) {
                return Err(gst::FlowError::Error);
            }
            let total_frames = buffer.size() / frame_bytes;
            let block_frames = (rate / 100).max(1);
            let mut start = 0;
            while start < total_frames {
                if self.flushing.load(Ordering::Acquire) {
                    return Err(gst::FlowError::Flushing);
                }
                let mut frames = block_frames.min(total_frames - start);
                if let Some(pts) = buffer.pts() {
                    let position = pts
                        .nseconds()
                        .saturating_add((start as u128 * 1_000_000_000 / rate as u128) as u64);
                    let guard = super::super::dynamic_gain::OPENING_GUARD_NS;
                    if position < guard {
                        let until_guard = ((guard - position) as u128 * rate as u128)
                            .div_ceil(1_000_000_000)
                            as usize;
                        frames = frames.min(until_guard.max(1));
                    }
                }
                let mut block = buffer
                    .copy_region(
                        gst::BUFFER_COPY_ALL,
                        start * frame_bytes..(start + frames) * frame_bytes,
                    )
                    .map_err(|_| gst::FlowError::Error)?;
                let time_at = |frame: usize| {
                    gst::ClockTime::from_nseconds(
                        (frame as u128 * 1_000_000_000 / rate as u128) as u64,
                    )
                };
                {
                    let writable = block.make_mut();
                    writable.set_pts(buffer.pts().map(|pts| pts + time_at(start)));
                    writable.set_dts(buffer.dts().map(|dts| dts + time_at(start)));
                    // Differences of endpoints avoid drift at e.g. 44.1kHz.
                    let duration = if start + frames == total_frames {
                        buffer
                            .duration()
                            .and_then(|duration| duration.checked_sub(time_at(start)))
                            .unwrap_or(time_at(start + frames) - time_at(start))
                    } else {
                        time_at(start + frames) - time_at(start)
                    };
                    writable.set_duration(duration);
                    if buffer.offset() != u64::MAX {
                        writable.set_offset(buffer.offset() + start as u64);
                        writable.set_offset_end(buffer.offset() + (start + frames) as u64);
                    }
                    if start != 0 {
                        writable.unset_flags(gst::BufferFlags::DISCONT);
                    }
                }
                {
                    let mut pending = self.pending.lock().unwrap();
                    let mut processor = self.processor.get().unwrap().lock().unwrap();
                    if pending.synchronize(&mut processor)? {
                        self.spectrum.get().unwrap().reset();
                        let _ = self.obj().post_message(
                            gst::message::Latency::builder().src(&*self.obj()).build(),
                        );
                    }
                    let target = processor.analyze_at(
                        block
                            .map_readable()
                            .map_err(|_| gst::FlowError::Error)?
                            .as_slice(),
                        Some(buffer_position.saturating_add(time_at(start).nseconds())),
                    );
                    pending.frames += frames;
                    pending.blocks.push_back(Pending {
                        buffer: block,
                        frames,
                        position_ns: buffer_position.saturating_add(time_at(start).nseconds()),
                        target,
                    });
                }
                self.emit(false)?;
                start += frames;
            }
            Ok(gst::FlowSuccess::Ok)
        }

        fn event(&self, event: gst::Event) -> bool {
            match event.view() {
                gst::EventView::FlushStart(_) => {
                    self.flushing.store(true, Ordering::Release);
                    // Unblock a pending downstream push before clearing local data.
                    let result = self.src.push_event(event);
                    self.reset();
                    return result;
                }
                gst::EventView::FlushStop(_) => {
                    self.reset();
                    self.flushing.store(false, Ordering::Release);
                }
                gst::EventView::StreamStart(_) | gst::EventView::Segment(_) => self.reset(),
                gst::EventView::Caps(caps) => {
                    if self.emit(true).is_err() {
                        return false;
                    }
                    let Some(s) = caps.caps().structure(0) else {
                        return false;
                    };
                    let (Ok(rate), Ok(channels)) = (s.get::<i32>("rate"), s.get::<i32>("channels"))
                    else {
                        return false;
                    };
                    let format = if cfg!(target_endian = "little") {
                        "F32LE"
                    } else {
                        "F32BE"
                    };
                    if rate <= 0
                        || channels <= 0
                        || s.get::<&str>("format").ok() != Some(format)
                        || s.get::<&str>("layout").ok() != Some("interleaved")
                    {
                        return false;
                    }
                    self.processor
                        .get()
                        .unwrap()
                        .lock()
                        .unwrap()
                        .set_format(rate as usize, channels as usize);
                }
                gst::EventView::Eos(_) => {
                    if self.emit(true).is_err() {
                        return false;
                    }
                    self.processor.get().unwrap().lock().unwrap().finish_song();
                }
                _ => {}
            }
            self.src.push_event(event)
        }

        fn forward_query(&self, peer: &gst::Pad, query: &mut gst::QueryRef) -> bool {
            let result = peer.peer_query(query);
            if let gst::QueryViewMut::Caps(caps) = query.view_mut() {
                let template = self.sink.pad_template_caps();
                let mut allowed = caps
                    .result()
                    .map_or_else(|| template.clone(), |result| result.intersect(&template));
                if let Some(filter) = caps.filter() {
                    allowed = allowed.intersect(filter);
                }
                caps.set_result(&allowed);
                return true;
            }
            result
        }

        fn src_query(&self, query: &mut gst::QueryRef) -> bool {
            if !self.forward_query(&self.sink, query) {
                return false;
            }
            if let gst::QueryViewMut::Latency(latency) = query.view_mut() {
                let (live, min, max) = latency.result();
                if live
                    && self
                        .processor
                        .get()
                        .is_some_and(|p| p.lock().unwrap().enabled())
                {
                    let delay = gst::ClockTime::from_mseconds(LOOKAHEAD_MS + 10);
                    latency.set(live, min + delay, max.map(|max| max + delay));
                }
            }
            true
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Lookahead {
        const NAME: &'static str = "StaticLookaheadGain";
        type Type = super::Lookahead;
        type ParentType = gst::Element;

        fn with_class(klass: &Self::Class) -> Self {
            let sink = gst::Pad::builder_from_template(&klass.pad_template("sink").unwrap())
                .chain_function(|_, parent, buffer| {
                    Self::catch_panic_pad_function(
                        parent,
                        || Err(gst::FlowError::Error),
                        |this| this.chain(buffer),
                    )
                })
                .event_function(|_, parent, event| {
                    Self::catch_panic_pad_function(parent, || false, |this| this.event(event))
                })
                .query_function(|_, parent, query| {
                    Self::catch_panic_pad_function(
                        parent,
                        || false,
                        |this| this.forward_query(&this.src, query),
                    )
                })
                .build();
            let src = gst::Pad::builder_from_template(&klass.pad_template("src").unwrap())
                .event_function(|_, parent, event| {
                    Self::catch_panic_pad_function(
                        parent,
                        || false,
                        |this| this.sink.push_event(event),
                    )
                })
                .query_function(|_, parent, query| {
                    Self::catch_panic_pad_function(parent, || false, |this| this.src_query(query))
                })
                .build();
            Self {
                processor: OnceLock::new(),
                spectrum: OnceLock::new(),
                isolation: OnceLock::new(),
                pending: Mutex::new(BufferedAudio::default()),
                flushing: AtomicBool::new(false),
                sink,
                src,
            }
        }
    }

    impl ObjectImpl for Lookahead {
        fn constructed(&self) {
            self.parent_constructed();
            self.obj().add_pad(&self.sink).unwrap();
            self.obj().add_pad(&self.src).unwrap();
        }
    }
    impl GstObjectImpl for Lookahead {}
    impl ElementImpl for Lookahead {
        fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
            static METADATA: OnceLock<gst::subclass::ElementMetadata> = OnceLock::new();
            Some(METADATA.get_or_init(|| {
                gst::subclass::ElementMetadata::new(
                    "Static lookahead gain",
                    "Filter/Effect/Audio",
                    "Buffered linked dynamic gain",
                    "Static",
                )
            }))
        }
        fn pad_templates() -> &'static [gst::PadTemplate] {
            static TEMPLATES: OnceLock<Vec<gst::PadTemplate>> = OnceLock::new();
            TEMPLATES.get_or_init(|| {
                let format = if cfg!(target_endian = "little") {
                    "F32LE"
                } else {
                    "F32BE"
                };
                let caps = gst::Caps::builder("audio/x-raw")
                    .field("format", format)
                    .field("layout", "interleaved")
                    .field("rate", gst::IntRange::new(1, i32::MAX))
                    .field("channels", gst::IntRange::new(1, i32::MAX))
                    .build();
                [
                    ("sink", gst::PadDirection::Sink),
                    ("src", gst::PadDirection::Src),
                ]
                .into_iter()
                .map(|(name, direction)| {
                    gst::PadTemplate::new(name, direction, gst::PadPresence::Always, &caps).unwrap()
                })
                .collect()
            })
        }
        fn change_state(
            &self,
            transition: gst::StateChange,
        ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
            let result = self.parent_change_state(transition)?;
            if matches!(
                transition,
                gst::StateChange::PausedToReady | gst::StateChange::ReadyToNull
            ) {
                self.reset();
                self.flushing.store(false, Ordering::Release);
            }
            Ok(result)
        }
    }
}

glib::wrapper! {
    pub struct Lookahead(ObjectSubclass<imp::Lookahead>) @extends gst::Element, gst::Object;
}

/// Native float processing precedes final audio conversion. The bounded queue
/// decodes ahead without moving timestamps or inserting silence into file audio.
pub(super) fn build(
    processor: Arc<Mutex<DynamicGain>>,
    spectrum: Arc<Spectrum>,
    isolation: Arc<Mutex<BandIsolator>>,
) -> Result<gst::Bin, glib::Error> {
    let bin = gst::Bin::new();
    let convert_in = gst::ElementFactory::make("audioconvert")
        .build()
        .map_err(|e| glib::Error::new(gst::CoreError::MissingPlugin, &e.to_string()))?;
    let convert_out = gst::ElementFactory::make("audioconvert")
        .build()
        .map_err(|e| glib::Error::new(gst::CoreError::MissingPlugin, &e.to_string()))?;
    let gain: Lookahead = glib::Object::builder()
        .property("name", "static_gain")
        .build();
    let _ = gain.imp().processor.set(processor);
    let _ = gain.imp().spectrum.set(spectrum);
    let _ = gain.imp().isolation.set(isolation);
    let add_error = |e: glib::BoolError| glib::Error::new(gst::CoreError::Failed, &e.to_string());
    bin.add_many([&convert_in, gain.upcast_ref(), &convert_out])
        .map_err(add_error)?;
    gst::Element::link_many([&convert_in, gain.upcast_ref(), &convert_out]).map_err(add_error)?;
    bin.add_pad(
        &gst::GhostPad::with_target(&convert_in.static_pad("sink").unwrap()).map_err(add_error)?,
    )
    .map_err(add_error)?;
    bin.add_pad(
        &gst::GhostPad::with_target(&convert_out.static_pad("src").unwrap()).map_err(add_error)?,
    )
    .map_err(add_error)?;
    Ok(bin)
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headless_filter_processes_real_gstreamer_buffers() {
        check_headless_filter(false);
    }

    #[test]
    fn headless_buffered_filter_prerolls_and_drains_short_audio_at_eos() {
        check_headless_filter(true);
    }

    fn check_headless_filter(enabled: bool) {
        gst::init().unwrap();
        let processor = Arc::new(Mutex::new(DynamicGain::default()));
        processor.lock().unwrap().master_gain = 5.0;
        processor
            .lock()
            .unwrap()
            .configure(super::super::dynamic_gain::Config {
                enabled,
                ..Default::default()
            });
        let spectrum = Arc::new(Spectrum::default());
        spectrum.frame(32, true);
        let filter = build(
            processor,
            spectrum.clone(),
            Arc::new(Mutex::new(BandIsolator::default())),
        )
        .unwrap();
        let pipeline = gst::Pipeline::new();
        let source = gst::ElementFactory::make("audiotestsrc")
            .property("num-buffers", 4i32)
            .property("volume", 0.1f64)
            .build()
            .unwrap();
        let sink = gst::ElementFactory::make("fakesink")
            .property("sync", false)
            .build()
            .unwrap();
        let observed_peak = Arc::new(Mutex::new(0.0_f32));
        let observed = observed_peak.clone();
        // Inspect the guaranteed F32 output before the sink's format negotiation.
        filter
            .by_name("static_gain")
            .unwrap()
            .static_pad("src")
            .unwrap()
            .add_probe(gst::PadProbeType::BUFFER, move |_, info| {
                if let Some(buffer) = info.buffer() {
                    let map = buffer.map_readable().unwrap();
                    let mut peak = observed.lock().unwrap();
                    for sample in map.as_slice().chunks_exact(4) {
                        *peak = peak.max(f32::from_ne_bytes(sample.try_into().unwrap()).abs());
                    }
                }
                gst::PadProbeReturn::Ok
            });
        pipeline
            .add_many([&source, filter.upcast_ref(), &sink])
            .unwrap();
        gst::Element::link_many([&source, filter.upcast_ref(), &sink]).unwrap();
        pipeline.set_state(gst::State::Playing).unwrap();
        let message = pipeline.bus().unwrap().timed_pop_filtered(
            gst::ClockTime::from_seconds(10),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        );
        let frame = spectrum.frame(32, true);
        pipeline.set_state(gst::State::Null).unwrap();
        assert!(
            matches!(
                message.as_ref().map(|m| m.view()),
                Some(gst::MessageView::Eos(_))
            ),
            "pipeline failed: {message:?}"
        );
        let peak = *observed_peak.lock().unwrap();
        assert!((0.49..0.51).contains(&peak), "master boost missing: {peak}");
        assert!(
            (0.49..0.51).contains(&frame.peak),
            "spectrum missed processed PCM: {}",
            frame.peak
        );
        assert!(frame.rms > 0.3, "spectrum must observe boosted audio");
        assert!(frame.bands.iter().any(|v| *v > 0.0));
    }
    struct Harness {
        gain: Lookahead,
        source: gst::Pad,
        sink: gst::Pad,
        output: Arc<Mutex<Vec<gst::Buffer>>>,
        processor: Arc<Mutex<DynamicGain>>,
    }

    impl Harness {
        fn new() -> Self {
            gst::init().unwrap();
            let processor = Arc::new(Mutex::new(DynamicGain::default()));
            processor
                .lock()
                .unwrap()
                .configure(super::super::dynamic_gain::Config {
                    enabled: true,
                    flatten_percent: 0.0,
                    change_lazyness_percent: 0.0,
                    ..Default::default()
                });
            let gain: Lookahead = glib::Object::builder().build();
            let _ = gain.imp().processor.set(processor.clone());
            let _ = gain.imp().spectrum.set(Arc::new(Spectrum::default()));
            let output = Arc::new(Mutex::new(Vec::new()));
            let captured = output.clone();
            let sink = gst::Pad::builder(gst::PadDirection::Sink)
                .chain_function(move |_, _, buffer| {
                    captured.lock().unwrap().push(buffer);
                    Ok(gst::FlowSuccess::Ok)
                })
                .event_function(|_, _, _| true)
                .build();
            let source = gst::Pad::builder(gst::PadDirection::Src)
                .query_function(|pad, parent, query| {
                    if let gst::QueryViewMut::Latency(latency) = query.view_mut() {
                        latency.set(true, gst::ClockTime::ZERO, None);
                        true
                    } else {
                        gst::Pad::query_default(pad, parent, query)
                    }
                })
                .build();
            source.set_active(true).unwrap();
            sink.set_active(true).unwrap();
            source.link(&gain.static_pad("sink").unwrap()).unwrap();
            gain.static_pad("src").unwrap().link(&sink).unwrap();
            gain.set_state(gst::State::Playing).unwrap();
            let harness = Self {
                gain,
                source,
                sink,
                output,
                processor,
            };
            harness.start();
            harness
        }

        fn start(&self) {
            assert!(self.source.push_event(gst::event::StreamStart::new("test")));
            let format = if cfg!(target_endian = "little") {
                "F32LE"
            } else {
                "F32BE"
            };
            let caps = gst::Caps::builder("audio/x-raw")
                .field("format", format)
                .field("layout", "interleaved")
                .field("rate", 1000i32)
                .field("channels", 2i32)
                .build();
            assert!(self.source.push_event(gst::event::Caps::new(&caps)));
            self.segment();
        }

        fn segment(&self) {
            assert!(
                self.source
                    .push_event(gst::event::Segment::new(&gst::FormattedSegment::<
                        gst::ClockTime,
                    >::new()))
            );
        }

        fn push(&self, amplitude: f32, frames: usize, start_ms: u64) {
            let bytes: Vec<u8> = (0..frames)
                .flat_map(|_| [amplitude, -amplitude / 2.0])
                .flat_map(f32::to_ne_bytes)
                .collect();
            let mut buffer = gst::Buffer::from_mut_slice(bytes);
            let writable = buffer.make_mut();
            writable.set_pts(gst::ClockTime::from_mseconds(start_ms));
            writable.set_dts(gst::ClockTime::from_mseconds(start_ms));
            writable.set_duration(gst::ClockTime::from_mseconds(frames as u64));
            writable.set_offset(start_ms);
            writable.set_offset_end(start_ms + frames as u64);
            self.source.push(buffer).unwrap();
        }

        fn eos(&self) {
            assert!(self.source.push_event(gst::event::Eos::new()));
        }
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            self.gain.set_state(gst::State::Null).unwrap();
            self.source.set_active(false).unwrap();
            self.sink.set_active(false).unwrap();
        }
    }

    #[test]
    fn buffered_filter_conserves_samples_timestamps_and_eos_tail() {
        let harness = Harness::new();
        harness.push(0.1, 123, 0);
        assert!(harness.output.lock().unwrap().is_empty());
        harness.push(0.1, 654, 123);
        harness.eos();
        let output = harness.output.lock().unwrap();
        let mut frames = 0;
        let mut duration = gst::ClockTime::ZERO;
        for buffer in output.iter() {
            assert_eq!(buffer.pts(), Some(duration));
            assert_eq!(buffer.dts(), Some(duration));
            assert_eq!(buffer.offset(), frames);
            frames += (buffer.size() / 8) as u64;
            assert_eq!(buffer.offset_end(), frames);
            duration += buffer.duration().unwrap();
        }
        assert_eq!(frames, 777);
        assert_eq!(duration, gst::ClockTime::from_mseconds(777));
    }

    #[test]
    fn buffered_filter_attenuates_before_upcoming_loud_entry() {
        let harness = Harness::new();
        harness.push(0.1, 1000, 15000);
        harness.push(0.6, 100, 16000);
        harness.eos();
        let output = harness.output.lock().unwrap();
        let samples: Vec<f32> = output
            .iter()
            .flat_map(|buffer| {
                buffer
                    .map_readable()
                    .unwrap()
                    .as_slice()
                    .chunks_exact(4)
                    .map(|s| f32::from_ne_bytes(s.try_into().unwrap()))
                    .collect::<Vec<_>>()
            })
            .collect();
        assert!(samples[990 * 2] < samples[700 * 2] / 3.0);
        assert!(samples[1000 * 2] < 0.15);
    }

    #[test]
    fn seek_flush_discards_old_audio_and_restarts_timestamps() {
        let harness = Harness::new();
        harness.push(0.8, 200, 0);
        assert!(harness.source.push_event(gst::event::FlushStart::new()));
        assert!(harness.source.push_event(gst::event::FlushStop::new(true)));
        harness.segment();
        harness.push(0.1, 101, 9000);
        harness.eos();
        let output = harness.output.lock().unwrap();
        assert_eq!(output.iter().map(|b| b.size() / 8).sum::<usize>(), 101);
        assert_eq!(output[0].pts(), Some(gst::ClockTime::from_seconds(9)));
        let map = output[0].map_readable().unwrap();
        assert_eq!(
            f32::from_ne_bytes(map.as_slice()[..4].try_into().unwrap()),
            0.1
        );
    }

    #[test]
    fn setting_change_reanalyzes_buffered_audio_without_dropping_samples() {
        let harness = Harness::new();
        harness.push(0.8, 200, 0);
        harness
            .processor
            .lock()
            .unwrap()
            .configure(Default::default());
        harness.push(0.1, 101, 200);
        harness.eos();
        let output = harness.output.lock().unwrap();
        assert_eq!(output.iter().map(|b| b.size() / 8).sum::<usize>(), 301);
        assert_eq!(output[0].pts(), Some(gst::ClockTime::ZERO));
        let map = output[0].map_readable().unwrap();
        assert_eq!(
            f32::from_ne_bytes(map.as_slice()[..4].try_into().unwrap()),
            0.8
        );
    }

    #[test]
    fn repeated_lazyness_updates_keep_audio_flowing_and_preserve_the_tail() {
        let harness = Harness::new();
        for step in 0..100 {
            harness
                .processor
                .lock()
                .unwrap()
                .configure(super::super::dynamic_gain::Config {
                    enabled: true,
                    flatten_percent: 0.0,
                    change_lazyness_percent: step as f64,
                    ..Default::default()
                });
            harness.push(0.1, 10, step * 10);
        }
        assert!(!harness.output.lock().unwrap().is_empty());
        harness.eos();
        let output = harness.output.lock().unwrap();
        assert_eq!(output.iter().map(|b| b.size() / 8).sum::<usize>(), 1000);
        let mut expected_pts = gst::ClockTime::ZERO;
        for buffer in output.iter() {
            assert_eq!(buffer.pts(), Some(expected_pts));
            expected_pts += buffer.duration().unwrap();
        }
        assert_eq!(expected_pts, gst::ClockTime::from_seconds(1));
    }

    #[test]
    fn gain_telemetry_follows_emitted_pts_and_keeps_history_on_seek() {
        let harness = Harness::new();
        harness.push(0.1, 250, 15000);
        assert_eq!(harness.processor.lock().unwrap().diagnostics().sequence, 0);
        harness.push(0.8, 10, 15250);
        let frame = harness.processor.lock().unwrap().diagnostics();
        assert_eq!(frame.position_millis, Some(15000));
        assert!((frame.input_peak - 0.1).abs() < 1e-6);
        assert!(frame.preview_gain < frame.requested_gain);
        let output = harness.output.lock().unwrap();
        let bytes = output.last().unwrap().map_readable().unwrap();
        let samples: Vec<f64> = bytes
            .as_slice()
            .chunks_exact(4)
            .map(|v| f32::from_ne_bytes(v.try_into().unwrap()) as f64)
            .collect();
        assert!(
            (frame.output_rms
                - (samples.iter().map(|v| v * v).sum::<f64>() / samples.len() as f64).sqrt())
            .abs()
                < 1e-9
        );
        drop(bytes);
        drop(output);
        harness.segment();
        let reset = harness.processor.lock().unwrap().diagnostics();
        assert_eq!(reset.sequence, frame.sequence);
        assert_eq!(reset.position_millis, frame.position_millis);
        assert_eq!(reset.rolling_rms, frame.rolling_rms);
        assert_eq!(reset.generation, frame.generation);
    }

    #[test]
    fn eos_finalizes_the_emitted_tail_and_skip_excludes_buffered_future_audio() {
        for natural_end in [false, true] {
            let harness = Harness::new();
            {
                let mut processor = harness.processor.lock().unwrap();
                processor.configure(super::super::dynamic_gain::Config {
                    enabled: true,
                    xdgain_enabled: true,
                    flatten_percent: 0.0,
                    ra_seconds: 1.0,
                    ..Default::default()
                });
            }
            harness.push(0.1, 260, 15000);
            if natural_end {
                harness.push(0.8, 100, 15260);
                harness.eos();
            } else {
                harness.push(0.8, 100, 15260);
            }
            let mut processor = harness.processor.lock().unwrap();
            let last_db = 20.0 * processor.diagnostics().rolling_rms.log10();
            assert_eq!(processor.diagnostics().xdgain_reference_dbfs, None);
            let state = processor.cross_song_state();
            processor.reset();
            processor.finish_song();
            drop(processor);
            let next = Harness::new();
            let mut new_processor = next.processor.lock().unwrap();
            new_processor.configure(super::super::dynamic_gain::Config {
                enabled: true,
                xdgain_enabled: true,
                ..Default::default()
            });
            new_processor.restore_cross_song_state(state);
            assert!(
                (new_processor.diagnostics().xdgain_reference_dbfs.unwrap() - last_db).abs()
                    < 1e-10
            );
        }
    }

    #[test]
    fn startup_boundary_is_split_and_quiet_intro_cannot_seed_wra() {
        let harness = Harness::new();
        harness.push(0.0005, 13, 14990); // crosses the 15-second boundary mid-buffer
        harness.push(0.2, 300, 15003);
        harness.eos();
        let output = harness.output.lock().unwrap();
        assert_eq!(output[0].pts(), Some(gst::ClockTime::from_mseconds(14990)));
        assert_eq!(
            output[0].duration(),
            Some(gst::ClockTime::from_mseconds(10))
        );
        assert_eq!(output[1].pts(), Some(gst::ClockTime::from_seconds(15)));
        let map = output[0].map_readable().unwrap();
        assert_eq!(
            f32::from_ne_bytes(map.as_slice()[..4].try_into().unwrap()),
            0.0005
        );
        assert_eq!(
            output.iter().map(|buffer| buffer.size() / 8).sum::<usize>(),
            313
        );
    }

    #[test]
    fn flush_seek_restores_recorded_wra_through_real_gstreamer_buffers() {
        let harness = Harness::new();
        harness.push(0.2, 1000, 15000);
        harness.push(0.02, 1000, 16000);
        harness.eos();
        let generation = harness.processor.lock().unwrap().diagnostics().generation;
        assert!(harness.source.push_event(gst::event::FlushStart::new()));
        assert!(harness.source.push_event(gst::event::FlushStop::new(true)));
        harness.segment();
        harness.push(0.2, 100, 15000);
        harness.eos();
        let frame = harness.processor.lock().unwrap().diagnostics();
        assert_eq!(frame.generation, generation);
        assert!((frame.rolling_rms - 0.2 * 0.625_f64.sqrt()).abs() < 1e-6);
    }

    #[test]
    fn live_latency_reports_lookahead_and_disabled_gain_has_no_extra_delay() {
        let harness = Harness::new();
        let mut query = gst::query::Latency::new();
        assert!(harness.sink.peer_query(&mut query));
        assert_eq!(query.result().1, gst::ClockTime::from_mseconds(260));
        harness
            .processor
            .lock()
            .unwrap()
            .configure(Default::default());
        assert!(harness.sink.peer_query(&mut query));
        assert_eq!(query.result().1, gst::ClockTime::ZERO);
    }
}
