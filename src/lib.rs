//! Skoll: a video sync plugin for Bitwig. See `docs/spec.md`.

use nih_plug::prelude::*;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, RwLock};

mod config;
mod host_link;
pub mod log;
mod mpv;
mod paths;
mod sync;
mod transport;
mod video_path;
mod worker;

use config::Config;
use transport::SharedTransport;
use worker::Worker;

/// Numbers plugin instances within one host process, so their log lines can be told apart.
static NEXT_INSTANCE: AtomicU32 = AtomicU32::new(1);

pub struct Skoll {
    params: Arc<SkollParams>,
    instance: u32,
    transport: Arc<SharedTransport>,
    _worker: Worker,
}

#[derive(Params)]
pub(crate) struct SkollParams {
    /// The song time in seconds at which the video's first frame appears.
    #[id = "offset"]
    pub offset: FloatParam,

    /// The video file. Plugin state, not a parameter: saved with the project.
    #[persist = "video-path"]
    pub video_path: RwLock<Option<String>>,
}

impl Default for SkollParams {
    fn default() -> Self {
        Self {
            offset: FloatParam::new(
                "Offset",
                0.0,
                FloatRange::Linear {
                    min: -3600.0,
                    max: 3600.0,
                },
            )
            .with_unit(" s")
            .with_step_size(0.001)
            .with_value_to_string(formatters::v2s_f32_rounded(3)),
            video_path: RwLock::new(None),
        }
    }
}

impl Default for Skoll {
    fn default() -> Self {
        let instance = NEXT_INSTANCE.fetch_add(1, Ordering::Relaxed);
        log!(instance, "created: {} {}", Self::NAME, Self::VERSION);

        let transport = Arc::new(SharedTransport::default());
        let params = Arc::new(SkollParams::default());
        Self {
            instance,
            _worker: Worker::spawn(instance, transport.clone(), params.clone(), Config::load),
            params,
            transport,
        }
    }
}

impl Drop for Skoll {
    fn drop(&mut self) {
        log!(self.instance, "destroyed");
    }
}

impl Plugin for Skoll {
    const NAME: &'static str = "Skoll";
    const VENDOR: &'static str = "Liminal Field";
    const URL: &'static str = env!("CARGO_PKG_HOMEPAGE");
    const EMAIL: &'static str = "";
    const VERSION: &'static str = env!("CARGO_PKG_VERSION");

    const AUDIO_IO_LAYOUTS: &'static [AudioIOLayout] = &[AudioIOLayout {
        main_input_channels: NonZeroU32::new(2),
        main_output_channels: NonZeroU32::new(2),
        ..AudioIOLayout::const_default()
    }];

    const MIDI_INPUT: MidiConfig = MidiConfig::None;
    const SAMPLE_ACCURATE_AUTOMATION: bool = false;

    type SysExMessage = ();
    type BackgroundTask = ();

    fn params(&self) -> Arc<dyn Params> {
        self.params.clone()
    }

    fn initialize(
        &mut self,
        _audio_io_layout: &AudioIOLayout,
        buffer_config: &BufferConfig,
        _context: &mut impl InitContext<Self>,
    ) -> bool {
        log!(
            self.instance,
            "initialized: sample rate {} Hz, max block {} samples",
            buffer_config.sample_rate,
            buffer_config.max_buffer_size
        );
        true
    }

    fn process(
        &mut self,
        _buffer: &mut Buffer,
        _aux: &mut AuxiliaryBuffers,
        context: &mut impl ProcessContext<Self>,
    ) -> ProcessStatus {
        // Audio passes through unchanged: the buffer is processed in place.
        // `pos_seconds()` prefers the host's seconds and falls back to samples / sample rate.
        let transport = context.transport();
        // `Instant::now()` is a vDSO clock read: no syscall, no allocation.
        self.transport.store(
            transport.playing,
            transport.pos_seconds(),
            std::time::Instant::now(),
            transport.sample_rate,
        );
        ProcessStatus::Normal
    }

    fn deactivate(&mut self) {
        log!(self.instance, "deactivated");
    }
}

impl ClapPlugin for Skoll {
    const CLAP_ID: &'static str = "com.liminalfield.skoll";
    const CLAP_DESCRIPTION: Option<&'static str> =
        Some("Syncs an mpv video window to the transport");
    const CLAP_MANUAL_URL: Option<&'static str> = Some(Self::URL);
    const CLAP_SUPPORT_URL: Option<&'static str> = None;
    const CLAP_FEATURES: &'static [ClapFeature] = &[
        ClapFeature::AudioEffect,
        ClapFeature::Stereo,
        ClapFeature::Utility,
    ];
}

impl Vst3Plugin for Skoll {
    // Never change this: hosts identify saved VST3 instances by it.
    const VST3_CLASS_ID: [u8; 16] = *b"SkollLiminalFld1";
    const VST3_SUBCATEGORIES: &'static [Vst3SubCategory] =
        &[Vst3SubCategory::Fx, Vst3SubCategory::Tools];
}

nih_export_clap!(Skoll);
nih_export_vst3!(Skoll);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_path_round_trips_through_saved_state() {
        let params = SkollParams::default();
        *params.video_path.write().unwrap() = Some("/videos/cue 3.mov".to_owned());
        let saved = params.serialize_fields();

        let restored = SkollParams::default();
        restored.deserialize_fields(&saved);
        assert_eq!(
            restored.video_path.read().unwrap().as_deref(),
            Some("/videos/cue 3.mov")
        );
    }
}
