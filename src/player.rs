use std::{
    fs::File,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use cpal::{
    Device, SampleFormat, Stream, StreamConfig,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use crossbeam_channel::{Receiver, Sender, bounded};
use symphonia::{
    core::{
        audio::sample::Sample,
        codecs::{audio::AudioDecoderOptions, registry::CodecRegistry},
        errors::Error as SymphoniaError,
        formats::{FormatOptions, SeekMode, SeekTo, TrackType, probe::Hint},
        io::MediaSourceStream,
        meta::MetadataOptions,
        units::Time,
    },
    default::{get_probe, register_enabled_codecs},
};
use symphonia_adapter_libopus::OpusDecoder;

const NO_SEEK: u64 = u64::MAX;
const MIN_SAMPLE_RATE: u32 = 8_000;
const MAX_SAMPLE_RATE: u32 = 192_000;
const MAX_OUTPUT_CHANNELS: usize = 8;
const PREBUFFER_MILLIS: usize = 100;

#[derive(Debug, Clone)]
pub enum PlayerEvent {
    Ended,
    Error(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackState {
    Stopped,
    Playing,
    Paused,
}

struct AtomicRing {
    cells: Box<[AtomicU32]>,
    capacity: usize,
    head: AtomicUsize,
    tail: AtomicUsize,
}

impl AtomicRing {
    fn new(capacity: usize) -> Self {
        let cells = (0..capacity)
            .map(|_| AtomicU32::new(0))
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Self {
            cells,
            capacity,
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
        }
    }

    fn push(&self, sample: f32) -> bool {
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);
        if head.wrapping_sub(tail) >= self.capacity {
            return false;
        }
        self.cells[head % self.capacity].store(sample.to_bits(), Ordering::Relaxed);
        self.head.store(head.wrapping_add(1), Ordering::Release);
        true
    }

    fn pop(&self) -> Option<f32> {
        let tail = self.tail.load(Ordering::Relaxed);
        let head = self.head.load(Ordering::Acquire);
        if tail == head {
            return None;
        }
        let sample = f32::from_bits(self.cells[tail % self.capacity].load(Ordering::Relaxed));
        self.tail.store(tail.wrapping_add(1), Ordering::Release);
        Some(sample)
    }

    fn clear(&self) {
        let head = self.head.load(Ordering::Acquire);
        self.tail.store(head, Ordering::Release);
    }

    fn is_empty(&self) -> bool {
        self.head.load(Ordering::Acquire) == self.tail.load(Ordering::Acquire)
    }

    fn len(&self) -> usize {
        self.head
            .load(Ordering::Acquire)
            .wrapping_sub(self.tail.load(Ordering::Acquire))
            .min(self.capacity)
    }
}

struct Playback {
    _stream: Stream,
    decoder: Option<JoinHandle<()>>,
    stop: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    volume_bits: Arc<AtomicU32>,
    seek_millis: Arc<AtomicU64>,
    position_frames: Arc<AtomicU64>,
    underflows: Arc<AtomicU64>,
    sample_rate: u32,
}

impl Playback {
    fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.decoder.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for Playback {
    fn drop(&mut self) {
        self.stop();
    }
}

pub struct AudioPlayer {
    preferred_device: Option<String>,
    current: Option<Playback>,
    events_tx: Sender<PlayerEvent>,
    events_rx: Receiver<PlayerEvent>,
}

impl AudioPlayer {
    pub fn new(preferred_device: Option<String>) -> Self {
        let (events_tx, events_rx) = bounded(16);
        Self {
            preferred_device,
            current: None,
            events_tx,
            events_rx,
        }
    }

    pub fn play(&mut self, path: impl AsRef<Path>) -> Result<()> {
        self.stop();
        while self.events_rx.try_recv().is_ok() {}
        let path = path.as_ref().to_path_buf();
        if !path.is_file() {
            bail!("arquivo nao encontrado: {}", path.display());
        }
        let device = output_device(self.preferred_device.as_deref())?;
        let supported = device
            .default_output_config()
            .context("dispositivo sem configuracao de saida")?;
        let sample_format = supported.sample_format();
        let config: StreamConfig = supported.into();
        let channels = config.channels as usize;
        let sample_rate = config.sample_rate.0;
        if !(MIN_SAMPLE_RATE..=MAX_SAMPLE_RATE).contains(&sample_rate) {
            bail!("sample rate de saida fora do limite seguro: {sample_rate} Hz");
        }
        if channels == 0 || channels > MAX_OUTPUT_CHANNELS {
            bail!("numero de canais de saida fora do limite seguro: {channels}");
        }
        let ring = Arc::new(AtomicRing::new(
            (sample_rate as usize * channels * 2).max(4096),
        ));
        let stop = Arc::new(AtomicBool::new(false));
        let paused = Arc::new(AtomicBool::new(false));
        let volume_bits = Arc::new(AtomicU32::new(0.8_f32.to_bits()));
        let seek_millis = Arc::new(AtomicU64::new(NO_SEEK));
        let position_frames = Arc::new(AtomicU64::new(0));
        let underflows = Arc::new(AtomicU64::new(0));
        let eos = Arc::new(AtomicBool::new(false));

        let decoder = spawn_decoder(DecoderContext {
            path,
            ring: Arc::clone(&ring),
            stop: Arc::clone(&stop),
            seek_millis: Arc::clone(&seek_millis),
            position_frames: Arc::clone(&position_frames),
            eos: Arc::clone(&eos),
            target_rate: sample_rate,
            target_channels: channels,
            events: self.events_tx.clone(),
        });
        let prebuffer_samples = sample_rate as usize * channels * PREBUFFER_MILLIS / 1000;
        let prebuffer_deadline = std::time::Instant::now() + Duration::from_secs(2);
        while ring.len() < prebuffer_samples
            && !eos.load(Ordering::Acquire)
            && std::time::Instant::now() < prebuffer_deadline
        {
            thread::sleep(Duration::from_millis(2));
        }
        let stream = match build_stream(
            &device,
            &config,
            sample_format,
            OutputContext {
                ring,
                stop: Arc::clone(&stop),
                paused: Arc::clone(&paused),
                volume_bits: Arc::clone(&volume_bits),
                position_frames: Arc::clone(&position_frames),
                underflows: Arc::clone(&underflows),
                eos,
                channels,
                events: self.events_tx.clone(),
            },
        ) {
            Ok(stream) => stream,
            Err(error) => {
                stop.store(true, Ordering::Release);
                let _ = decoder.join();
                return Err(error);
            }
        };
        if let Err(error) = stream.play() {
            stop.store(true, Ordering::Release);
            let _ = decoder.join();
            return Err(error).context("falha ao iniciar dispositivo de audio");
        }
        self.current = Some(Playback {
            _stream: stream,
            decoder: Some(decoder),
            stop,
            paused,
            volume_bits,
            seek_millis,
            position_frames,
            underflows,
            sample_rate,
        });
        Ok(())
    }

    pub fn stop(&mut self) {
        if let Some(mut playback) = self.current.take() {
            playback.stop();
        }
    }

    pub fn toggle_pause(&self) -> Option<PlaybackState> {
        let playback = self.current.as_ref()?;
        let paused = !playback.paused.load(Ordering::Relaxed);
        playback.paused.store(paused, Ordering::Relaxed);
        Some(if paused {
            PlaybackState::Paused
        } else {
            PlaybackState::Playing
        })
    }

    pub fn state(&self) -> PlaybackState {
        match &self.current {
            None => PlaybackState::Stopped,
            Some(playback) if playback.paused.load(Ordering::Relaxed) => PlaybackState::Paused,
            Some(_) => PlaybackState::Playing,
        }
    }

    pub fn is_active(&self) -> bool {
        self.current.is_some()
    }

    pub fn set_volume(&self, volume: f32) {
        if let Some(playback) = &self.current {
            playback
                .volume_bits
                .store(volume.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
        }
    }

    pub fn volume(&self) -> f32 {
        self.current
            .as_ref()
            .map(|playback| f32::from_bits(playback.volume_bits.load(Ordering::Relaxed)))
            .unwrap_or(0.8)
    }

    pub fn seek_relative(&self, seconds: i64) {
        if let Some(playback) = &self.current {
            let current = self.position().as_millis() as i128;
            let target = (current + i128::from(seconds) * 1000).max(0) as u64;
            playback.seek_millis.store(target, Ordering::Release);
        }
    }

    pub fn position(&self) -> Duration {
        self.current
            .as_ref()
            .map(|playback| {
                let frames = playback.position_frames.load(Ordering::Relaxed);
                Duration::from_secs_f64(frames as f64 / playback.sample_rate as f64)
            })
            .unwrap_or_default()
    }

    pub fn underflows(&self) -> u64 {
        self.current
            .as_ref()
            .map(|playback| playback.underflows.load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    pub fn reset_underflows(&self) {
        if let Some(playback) = &self.current {
            playback.underflows.store(0, Ordering::Relaxed);
        }
    }

    pub fn try_events(&self) -> impl Iterator<Item = PlayerEvent> + '_ {
        self.events_rx.try_iter()
    }

    pub fn try_event(&self) -> Option<PlayerEvent> {
        self.events_rx.try_recv().ok()
    }

    pub fn device_names() -> Result<Vec<String>> {
        let host = cpal::default_host();
        let devices = host.output_devices().context("falha ao listar saidas")?;
        Ok(devices.filter_map(|device| device.name().ok()).collect())
    }
}

impl Drop for AudioPlayer {
    fn drop(&mut self) {
        self.stop();
    }
}

fn output_device(preferred: Option<&str>) -> Result<Device> {
    let host = cpal::default_host();
    if let Some(preferred) = preferred
        && let Some(device) = host
            .output_devices()
            .context("falha ao listar dispositivos")?
            .find(|device| device.name().map(|name| name == preferred).unwrap_or(false))
    {
        return Ok(device);
    }
    host.default_output_device()
        .context("nenhum dispositivo de audio encontrado")
}

struct OutputContext {
    ring: Arc<AtomicRing>,
    stop: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    volume_bits: Arc<AtomicU32>,
    position_frames: Arc<AtomicU64>,
    underflows: Arc<AtomicU64>,
    eos: Arc<AtomicBool>,
    channels: usize,
    events: Sender<PlayerEvent>,
}

fn build_stream(
    device: &Device,
    config: &StreamConfig,
    format: SampleFormat,
    context: OutputContext,
) -> Result<Stream> {
    let error_events = context.events.clone();
    let error_callback = move |error| {
        let _ = error_events.try_send(PlayerEvent::Error(format!("audio: {error}")));
    };
    let stream = match format {
        SampleFormat::F32 => {
            let mut ended = false;
            device.build_output_stream(
                config,
                move |data: &mut [f32], _| render(data, &context, &mut ended, |sample| sample),
                error_callback,
                None,
            )?
        }
        SampleFormat::I16 => {
            let mut ended = false;
            device.build_output_stream(
                config,
                move |data: &mut [i16], _| {
                    render(data, &context, &mut ended, |sample| {
                        (sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16
                    })
                },
                error_callback,
                None,
            )?
        }
        SampleFormat::U16 => {
            let mut ended = false;
            device.build_output_stream(
                config,
                move |data: &mut [u16], _| {
                    render(data, &context, &mut ended, |sample| {
                        ((sample.clamp(-1.0, 1.0) + 1.0) * 0.5 * u16::MAX as f32) as u16
                    })
                },
                error_callback,
                None,
            )?
        }
        other => bail!("formato de amostra nao suportado: {other:?}"),
    };
    Ok(stream)
}

fn render<T: Copy>(
    output: &mut [T],
    context: &OutputContext,
    ended_sent: &mut bool,
    convert: impl Fn(f32) -> T,
) {
    let silence = convert(0.0);
    if context.stop.load(Ordering::Relaxed) || context.paused.load(Ordering::Relaxed) {
        output.fill(silence);
        return;
    }
    let volume = f32::from_bits(context.volume_bits.load(Ordering::Relaxed));
    let mut missing = false;
    let mut consumed = 0_u64;
    for sample in output.iter_mut() {
        if let Some(value) = context.ring.pop() {
            *sample = convert(value * volume);
            consumed += 1;
        } else {
            *sample = silence;
            missing = true;
        }
    }
    context
        .position_frames
        .fetch_add(consumed / context.channels.max(1) as u64, Ordering::Relaxed);
    if missing && !context.eos.load(Ordering::Acquire) {
        context.underflows.fetch_add(1, Ordering::Relaxed);
    }
    if !*ended_sent && context.eos.load(Ordering::Acquire) && context.ring.is_empty() {
        *ended_sent = true;
        let _ = context.events.try_send(PlayerEvent::Ended);
    }
}

struct DecoderContext {
    path: PathBuf,
    ring: Arc<AtomicRing>,
    stop: Arc<AtomicBool>,
    seek_millis: Arc<AtomicU64>,
    position_frames: Arc<AtomicU64>,
    eos: Arc<AtomicBool>,
    target_rate: u32,
    target_channels: usize,
    events: Sender<PlayerEvent>,
}

fn spawn_decoder(context: DecoderContext) -> JoinHandle<()> {
    thread::spawn(move || {
        if let Err(error) = decode_loop(&context) {
            context.eos.store(true, Ordering::Release);
            if !context.stop.load(Ordering::Relaxed) {
                let _ = context
                    .events
                    .try_send(PlayerEvent::Error(format!("{error:#}")));
            }
        }
    })
}

fn decode_loop(context: &DecoderContext) -> Result<()> {
    let file = Box::new(
        File::open(&context.path)
            .with_context(|| format!("falha ao abrir {}", context.path.display()))?,
    );
    let mss = MediaSourceStream::new(file, Default::default());
    let mut hint = Hint::new();
    if let Some(extension) = context.path.extension().and_then(|value| value.to_str()) {
        hint.with_extension(extension);
    }
    let mut format = get_probe()
        .probe(
            &hint,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .context("formato de audio nao reconhecido")?;
    let track = format
        .default_track(TrackType::Audio)
        .context("arquivo sem faixa de audio")?;
    let track_id = track.id;
    let codec_params = track
        .codec_params
        .as_ref()
        .and_then(|params| params.audio())
        .context("parametros do codec ausentes")?
        .clone();
    let mut codecs = CodecRegistry::new();
    register_enabled_codecs(&mut codecs);
    codecs.register_audio_decoder::<OpusDecoder>();
    let mut decoder = codecs
        .make_audio_decoder(&codec_params, &AudioDecoderOptions::default())
        .context("codec nao suportado")?;
    let mut samples = Vec::<f32>::new();
    let mut resampler = LinearResampler::default();

    while !context.stop.load(Ordering::Relaxed) {
        let requested_seek = context.seek_millis.swap(NO_SEEK, Ordering::AcqRel);
        if requested_seek != NO_SEEK {
            let time = Time::from_millis_u64(requested_seek);
            format
                .seek(
                    SeekMode::Accurate,
                    SeekTo::Time {
                        time,
                        track_id: Some(track_id),
                    },
                )
                .context("seek nao suportado para esta faixa")?;
            decoder.reset();
            context.ring.clear();
            context.position_frames.store(
                requested_seek.saturating_mul(context.target_rate as u64) / 1000,
                Ordering::Relaxed,
            );
            resampler = LinearResampler::default();
        }

        let packet = match format.next_packet() {
            Ok(Some(packet)) => packet,
            Ok(None) => break,
            Err(SymphoniaError::ResetRequired) => bail!("stream exigiu reinicializacao"),
            Err(error) => return Err(error).context("falha ao ler pacote"),
        };
        if packet.track_id != track_id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(decoded) => decoded,
            Err(SymphoniaError::DecodeError(_)) | Err(SymphoniaError::IoError(_)) => continue,
            Err(error) => return Err(error).context("falha ao decodificar audio"),
        };
        let spec = decoded.spec();
        if !(MIN_SAMPLE_RATE..=384_000).contains(&spec.rate())
            || spec.channels().count() == 0
            || spec.channels().count() > 32
        {
            bail!(
                "formato de origem fora do limite seguro: {} Hz, {} canais",
                spec.rate(),
                spec.channels().count()
            );
        }
        samples.resize(decoded.samples_interleaved(), f32::MID);
        decoded.copy_to_slice_interleaved(&mut samples);
        resampler.push(&samples, spec.channels().count(), spec.rate(), context)?;
    }
    context.eos.store(true, Ordering::Release);
    Ok(())
}

#[derive(Default)]
struct LinearResampler {
    source_frame: u64,
    next_output_position: f64,
    previous: [f32; 2],
    has_previous: bool,
}

impl LinearResampler {
    fn push(
        &mut self,
        samples: &[f32],
        source_channels: usize,
        source_rate: u32,
        context: &DecoderContext,
    ) -> Result<()> {
        if source_channels == 0 || source_rate == 0 {
            bail!("especificacao de audio invalida");
        }
        let output_step = source_rate as f64 / context.target_rate as f64;
        for frame in samples.chunks_exact(source_channels) {
            let current = stereo_frame(frame);
            if !self.has_previous {
                self.previous = current;
                self.has_previous = true;
            }
            let current_position = self.source_frame as f64;
            let previous_position = current_position.saturating_sub(1.0);
            while self.next_output_position <= current_position {
                let fraction = if current_position > previous_position {
                    (self.next_output_position - previous_position).clamp(0.0, 1.0) as f32
                } else {
                    1.0
                };
                let left = self.previous[0] + (current[0] - self.previous[0]) * fraction;
                let right = self.previous[1] + (current[1] - self.previous[1]) * fraction;
                push_output_frame(context, left, right)?;
                self.next_output_position += output_step;
            }
            self.previous = current;
            self.source_frame = self.source_frame.saturating_add(1);
        }
        Ok(())
    }
}

trait SaturatingSubF64 {
    fn saturating_sub(self, rhs: f64) -> f64;
}

impl SaturatingSubF64 for f64 {
    fn saturating_sub(self, rhs: f64) -> f64 {
        (self - rhs).max(0.0)
    }
}

fn stereo_frame(frame: &[f32]) -> [f32; 2] {
    match frame {
        [] => [0.0, 0.0],
        [mono] => [*mono, *mono],
        [left, right, ..] => [*left, *right],
    }
}

fn push_output_frame(context: &DecoderContext, left: f32, right: f32) -> Result<()> {
    if context.target_channels == 1 {
        push_sample(context, (left + right) * 0.5)?;
    } else {
        push_sample(context, left)?;
        push_sample(context, right)?;
        for _ in 2..context.target_channels {
            push_sample(context, 0.0)?;
        }
    }
    Ok(())
}

fn push_sample(context: &DecoderContext, sample: f32) -> Result<()> {
    loop {
        if context.stop.load(Ordering::Relaxed) {
            bail!("reproducao encerrada");
        }
        if context.seek_millis.load(Ordering::Acquire) != NO_SEEK {
            return Ok(());
        }
        if context.ring.push(sample) {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_preserves_order_and_capacity() {
        let ring = AtomicRing::new(2);
        assert!(ring.push(0.25));
        assert!(ring.push(-0.5));
        assert!(!ring.push(1.0));
        assert_eq!(ring.pop(), Some(0.25));
        assert_eq!(ring.pop(), Some(-0.5));
        assert_eq!(ring.pop(), None);
    }

    #[test]
    fn converts_mono_and_stereo_frames() {
        assert_eq!(stereo_frame(&[0.5]), [0.5, 0.5]);
        assert_eq!(stereo_frame(&[0.2, -0.2]), [0.2, -0.2]);
    }

    #[test]
    fn new_player_reports_stopped_state() {
        let player = AudioPlayer::new(None);
        assert_eq!(player.state(), PlaybackState::Stopped);
        assert!(!player.is_active());
        assert_eq!(player.toggle_pause(), None);
    }
}
