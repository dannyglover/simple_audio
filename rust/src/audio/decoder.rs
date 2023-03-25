// This file is a part of simple_audio
// Copyright (c) 2022-2023 Erikas Taroza <erikastaroza@gmail.com>
//
// This program is free software: you can redistribute it and/or
// modify it under the terms of the GNU Lesser General Public License as
// published by the Free Software Foundation, either version 3 of
// the License, or (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of 
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.
// See the GNU Lesser General Public License for more details.
//
// You should have received a copy of the GNU Lesser General Public License along with this program.
// If not, see <https://www.gnu.org/licenses/>.

use std::{thread::{JoinHandle, self}, borrow::Cow};

use anyhow::{Context, anyhow};
use cpal::traits::StreamTrait;
use crossbeam::channel::Receiver;
use symphonia::{core::{formats::{FormatOptions, FormatReader, SeekTo, SeekMode}, meta::MetadataOptions, io::{MediaSourceStream, MediaSource}, probe::Hint, units::{Time, TimeBase}, audio::{AudioBufferRef, AudioBuffer}}, default};

use crate::utils::{progress_state_stream::*, playback_state_stream::update_playback_state_stream, types::*, callback_stream::update_callback_stream};

use super::{cpal_output::CpalOutput, controls::*};

pub struct Decoder
{
    rx: Receiver<ThreadMessage>,
    state: DecoderState,
    cpal_output: Option<CpalOutput>,
    playback: Option<Playback>,
    preload_playback: Option<Playback>,
    /// The `JoinHandle` for the thread that preloads a file.
    preload_thread: Option<JoinHandle<anyhow::Result<Playback>>>
}

impl Decoder
{
    /// Creates a new decoder.
    pub fn new() -> Self
    {
        let rx = TXRX.read().unwrap().1.clone();

        Decoder {
            rx,
            state: DecoderState::Idle,
            cpal_output: None,
            playback: None,
            preload_playback: None,
            preload_thread: None
        }
    }

    /// Starts decoding in an infinite loop.
    /// Listens for any incoming `ThreadMessage`s.
    /// 
    /// If playing, then the decoder decodes packets
    /// until the file is done playing.
    /// 
    /// If stopped, the decoder goes into an idle state
    /// where it waits for a message to come.
    pub fn start(mut self)
    {
        loop
        {
            // Check if the preload thread is done.
            match self.poll_preload_thread() {
                Err(_) => {
                    update_callback_stream(Callback::DecodeError);
                },
                _ => ()
            }

            // Check for incoming `ThreadMessage`s.
            match self.listen_for_message() {
                Ok(should_break) => if should_break {
                    break;
                },
                Err(_) => {
                    update_callback_stream(Callback::DecodeError);
                }
            }

            if self.state.is_idle() || self.state.is_paused() { continue; }

            // Decode and output the samples.
            match self.do_playback() {
                Ok(playback_complete) => if playback_complete {
                    self.state = DecoderState::Idle;
                    self.finish_playback();
                },
                Err(_) => {
                    update_callback_stream(Callback::DecodeError);
                }
            }
        }
    }

    /// Listens to `self.rx` for any incoming messages.
    /// 
    /// Blocks if the `self.state` is `Idle` or `Paused`.
    /// 
    /// Returns true if the `Dispose` message was received.
    /// Returns false otherwise.
    fn listen_for_message(&mut self) -> anyhow::Result<bool>
    {
        // If the player is paused, then block this thread until a message comes in
        // to save the CPU.
        let recv: Option<ThreadMessage> = if self.state.is_idle() || self.state.is_paused() {
            self.rx.recv().ok()
        } else {
            self.rx.try_recv().ok()
        };

        match recv
        {
            None => (),
            Some(message) => match message
            {
                ThreadMessage::Dispose => return Ok(true),
                ThreadMessage::Open(source) => {
                    self.cpal_output = None;
                    self.playback = Some(Self::open(source)?);
                },
                ThreadMessage::Play => {
                    self.state = DecoderState::Playing;

                    // Windows handles play/pause differently.
                    if cfg!(not(target_os = "windows")) {
                        if let Some(cpal_output) = &self.cpal_output {
                            cpal_output.stream.play()?;
                        }
                    }
                },
                ThreadMessage::Pause => {
                    self.state = DecoderState::Paused;

                    // Windows handles play/pause differently.
                    if cfg!(not(target_os = "windows")) {
                        if let Some(cpal_output) = &self.cpal_output {
                            cpal_output.stream.pause()?;
                        }
                    }
                },
                ThreadMessage::Stop => {
                    self.state = DecoderState::Idle;
                    self.cpal_output = None;
                    self.playback = None;
                },
                // When the device is changed/disconnected,
                // then we should reestablish a connection.
                // To make a new connection, dispose of the current cpal_output
                // and pause playback. Once the user is ready, they can start
                // playback themselves.
                ThreadMessage::DeviceChanged => {
                    self.cpal_output = None;
                    crate::Player::internal_pause();
                },
                ThreadMessage::Preload(source) => {
                    self.preload_playback = None;
                    IS_FILE_PRELOADED.store(false, std::sync::atomic::Ordering::SeqCst);
                    let handle = Self::preload(source);
                    self.preload_thread = Some(handle);
                },
                ThreadMessage::PlayPreload => {
                    if self.preload_playback.is_none() {
                        return Ok(false);
                    }

                    crate::Player::internal_play();

                    self.cpal_output = None;
                    self.playback = self.preload_playback.take();
                    IS_FILE_PRELOADED.store(false, std::sync::atomic::Ordering::SeqCst);
                }
            }
        }

        Ok(false)
    }

    /// Decodes a packet and writes to `cpal_output`.
    /// 
    /// Returns `true` when the playback is complete.
    /// Returns `false` otherwise.
    fn do_playback(&mut self) -> anyhow::Result<bool>
    {
        if self.playback.is_none() { return Ok(false); }
        let playback = self.playback.as_mut().unwrap();

        // If there is audio already decoded from preloading,
        // then output that instead.
        if let Some(preload) = playback.preload.take() {
            // Write the decoded packet to CPAL.
            if self.cpal_output.is_none()
            {
                let spec = *preload.spec();
                let duration = preload.capacity() as u64;
                self.cpal_output.replace(CpalOutput::new(spec, duration)?);
            }

            let buffer_ref = AudioBufferRef::F32(Cow::Borrowed(&preload));
            self.cpal_output.as_mut().unwrap().write(buffer_ref);

            return Ok(false);
        }

        let seek_ts: u64 = if let Some(seek_ts) = *SEEK_TS.read().unwrap()
        {
            let seek_to = SeekTo::Time { time: Time::from(seek_ts), track_id: Some(playback.track_id) };
            match playback.reader.seek(SeekMode::Coarse, seek_to)
            {
                Ok(seeked_to) => seeked_to.required_ts,
                Err(_) => 0
            }
        } else { 0 };

        // Clean up seek stuff.
        if SEEK_TS.read().unwrap().is_some()
        {
            *SEEK_TS.write().unwrap() = None;
            playback.decoder.reset();
            // Clear the ring buffer which prevents the writer
            // from blocking.
            if let Some(cpal_output) = self.cpal_output.as_ref() {
                cpal_output.ring_buffer_reader.skip_all();
            }
            return Ok(false);
        }

        // Decode the next packet.
        let packet = match playback.reader.next_packet()
        {
            Ok(packet) => packet,
            // An error occurs when the stream
            // has reached the end of the audio.
            Err(_) => {
                if IS_LOOPING.load(std::sync::atomic::Ordering::SeqCst)
                {
                    *SEEK_TS.write().unwrap() = Some(0);
                    crate::utils::callback_stream::update_callback_stream(Callback::PlaybackLooped);
                    return Ok(false);
                }

                return Ok(true);
            }
        };

        if packet.track_id() != playback.track_id { return Ok(false); }

        let decoded = playback.decoder.decode(&packet)
            .context("Could not decode audio packet.")?;

        if packet.ts() < seek_ts { return Ok(false); }
        
        let position = if let Some(timebase) = playback.timebase {
            timebase.calc_time(packet.ts()).seconds
        } else {
            0
        };

        // Update the progress stream with calculated times.
        let progress = ProgressState {
            position,
            duration: playback.duration
        };

        update_progress_state_stream(progress);
        *PROGRESS.write().unwrap() = progress;

        // Write the decoded packet to CPAL.
        if self.cpal_output.is_none()
        {
            let spec = *decoded.spec();
            let duration = decoded.capacity() as u64;
            self.cpal_output.replace(CpalOutput::new(spec, duration)?);
        }

        self.cpal_output.as_mut().unwrap().write(decoded);

        Ok(false)
    }

    /// Called when the file is finished playing.
    /// 
    /// Flushes `cpal_output` and sends a `Done` message to Dart.
    fn finish_playback(&mut self)
    {
        if let Some(cpal_output) = self.cpal_output.as_mut() {
            cpal_output.flush();
        }

        // Send the done message once cpal finishes flushing.
        // There may be samples left over and we don't want to
        // start playing another file before they are read.
        update_playback_state_stream(PlaybackState::Done);
        update_progress_state_stream(ProgressState { position: 0, duration: 0 });
        *PROGRESS.write().unwrap() = ProgressState { position: 0, duration: 0 };
        IS_PLAYING.store(false, std::sync::atomic::Ordering::SeqCst);
        crate::metadata::set_playback_state(PlaybackState::Done);
    }

    /// Opens the given source for playback. Returns a `Playback`
    /// for the source.
    fn open(source: Box<dyn MediaSource>) -> anyhow::Result<Playback>
    {
        let mss = MediaSourceStream::new(source, Default::default());
        let format_options = FormatOptions { enable_gapless: true, ..Default::default() };
        let metadata_options: MetadataOptions = Default::default();

        let probed = default::get_probe().format(
            &Hint::new(),
            mss,
            &format_options,
            &metadata_options
        ).context("Failed to create format reader.")?;

        let reader = probed.format;

        let track = reader.default_track()
            .context("Cannot start playback. There are no tracks present in the file.")?;
        let track_id = track.id;

        let decoder = default::get_codecs()
            .make(&track.codec_params, &Default::default())?;

        // Used only for outputting the current position and duration.
        let timebase = track.codec_params.time_base;
        let ts = track.codec_params.n_frames.map(|frames| track.codec_params.start_ts + frames);

        let duration = if let Some(timebase) = timebase {
            timebase.calc_time(ts.unwrap()).seconds
        } else {
            0
        };

        Ok(Playback {
            reader,
            decoder,
            track_id,
            timebase,
            duration,
            preload: None
        })
    }

    /// Spawns a thread that decodes the first packet of the source.
    /// 
    /// Returns a preloaded `Playback` when complete.
    fn preload(source: Box<dyn MediaSource>) -> JoinHandle<anyhow::Result<Playback>>
    {
        thread::spawn(move || {
            let mut playback = Self::open(source)?;
            // Preload
            let packet = playback.reader.next_packet()?;
            let buf_ref = playback.decoder.decode(&packet)?;

            let spec = *buf_ref.spec();
            let duration = buf_ref.capacity() as u64;

            let mut buf = AudioBuffer::new(duration, spec);
            buf_ref.convert(&mut buf);
            playback.preload = Some(buf);

            Ok(playback)
        })
    }

    /// Polls the `preload_thread`. If it is finished, the 
    /// preloaded file is then placed in `preload_playback`.
    fn poll_preload_thread(&mut self) -> anyhow::Result<()>
    {
        if self.preload_thread.is_none() || !self.preload_thread.as_ref().unwrap().is_finished() {
            return Ok(());
        }

        let handle = self.preload_thread.take().unwrap();
        let result = handle.join()
            .unwrap_or(Err(anyhow!("Could not join preload thread.")))?;

        self.preload_playback.replace(result);
        IS_FILE_PRELOADED.store(true, std::sync::atomic::Ordering::SeqCst);

        Ok(())
    }
}

enum DecoderState
{
    Playing,
    Paused,
    Idle
}

impl DecoderState
{
    fn is_idle(&self) -> bool {
        if let DecoderState::Idle = self {
            return true;
        }

        false
    }

    fn is_paused(&self) -> bool {
        if let DecoderState::Paused = self {
            return true;
        }

        false
    }
}

/// Holds the items related to playback.
/// 
/// Ex: The Symphonia decoder, timebase, duration.
struct Playback
{
    reader: Box<dyn FormatReader>,
    track_id: u32,
    decoder: Box<dyn symphonia::core::codecs::Decoder>,
    timebase: Option<TimeBase>,
    duration: u64,
    /// A buffer of already decoded samples.
    preload: Option<AudioBuffer<f32>>
}