use std::sync::{Arc, Mutex};

use gst::prelude::*;
use gstreamer as gst;

use super::dynamic_gain::DynamicGain;

/// The float format keeps both linked dynamic gain and master gain before the
/// final audio conversion. Playbin's own volume stays at unity.
pub(super) fn build(processor: Arc<Mutex<DynamicGain>>) -> Result<gst::Bin, gst::glib::Error> {
    let format = if cfg!(target_endian = "little") {
        "F32LE"
    } else {
        "F32BE"
    };
    let bin = gst::parse::bin_from_description(
        &format!(
            "audioconvert ! audio/x-raw,format={format},layout=interleaved ! identity name=static_gain ! audioconvert"
        ),
        true,
    )?;
    let pad = bin
        .by_name("static_gain")
        .unwrap()
        .static_pad("src")
        .unwrap();
    pad.add_probe(
        gst::PadProbeType::BUFFER
            | gst::PadProbeType::EVENT_DOWNSTREAM
            | gst::PadProbeType::EVENT_FLUSH,
        move |_, info| {
            let Ok(mut processor) = processor.lock() else {
                return gst::PadProbeReturn::Drop;
            };
            if let Some(event) = info.event() {
                match event.view() {
                    gst::EventView::Caps(caps) => {
                        if let Some(s) = caps.caps().structure(0) {
                            if let (Ok(rate), Ok(channels)) =
                                (s.get::<i32>("rate"), s.get::<i32>("channels"))
                            {
                                if rate > 0 && channels > 0 {
                                    processor.set_format(rate as usize, channels as usize);
                                }
                            }
                        }
                    }
                    gst::EventView::StreamStart(_)
                    | gst::EventView::Segment(_)
                    | gst::EventView::FlushStop(_) => processor.reset(),
                    _ => {}
                }
            }
            if let Some(buffer) = info.buffer_mut() {
                if buffer.flags().contains(gst::BufferFlags::DISCONT) {
                    processor.reset();
                }
                let buffer = buffer.make_mut();
                if let Ok(mut map) = buffer.map_writable() {
                    processor.process(map.as_mut_slice());
                } else {
                    // Never silently pass unprocessed audio at an unknown gain.
                    return gst::PadProbeReturn::Drop;
                }
            }
            gst::PadProbeReturn::Ok
        },
    );
    Ok(bin)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headless_filter_processes_real_gstreamer_buffers() {
        gst::init().unwrap();
        let processor = Arc::new(Mutex::new(DynamicGain::default()));
        processor.lock().unwrap().master_gain = 5.0;
        let filter = build(processor).unwrap();
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
    }
}
