use symphonia::{core::{formats::{FormatOptions, FormatReader, SeekTo, SeekMode}, meta::MetadataOptions, io::{MediaSourceStream, MediaSource}, probe::Hint, units::Time}, default};

use crate::dart_streams::progress_state_stream::*;

use super::{cpal_output::CpalOutput, controls::*};

#[derive(Default)]
pub struct Decoder;

impl Decoder
{
    pub fn open_stream(&mut self, source:Box<dyn MediaSource>)
    {
        let mss = MediaSourceStream::new(source, Default::default());

        let format_options = FormatOptions { enable_gapless: true, ..Default::default() };
        let metadata_options:MetadataOptions = Default::default();

        match default::get_probe().format(&Hint::new(), mss, &format_options, &metadata_options)
        {
            Err(err) => panic!("ERR: Failed to probe source. {err}"),
            Ok(mut probed) => self.decode_loop(&mut probed.format)
        }
    }

    fn decode_loop(&mut self, reader:&mut Box<dyn FormatReader>)
    {
        let track = reader.default_track().unwrap();
        let track_id = track.id;

        let mut decoder = default::get_codecs().make(&track.codec_params, &Default::default()).unwrap();
        let mut cpal_output:Option<CpalOutput> = None;

        // Used only for outputting the current position and duration.
        let timebase = track.codec_params.time_base.unwrap();
        let duration = track.codec_params.n_frames.map(|frames| track.codec_params.start_ts + frames).unwrap();

        // Clone a receiver to listen for the stop signal.
        let rx = TXRX.read().unwrap();
        let rx = rx.as_ref().unwrap().1.clone();

        loop
        {
            // Poll the status of the RX in lib.rs.
            // If the value is true, that means we want to stop this stream.
            // Breaking the loop drops everything which stops the cpal stream.
            let result = rx.try_recv();
            match result
            {
                Err(_) => (),
                Ok(message) => if message { break; }
            }

            // Seeking.
            let seek_ts:u64 = if let Some(seek_ts) = *SEEK_TS.read().unwrap()
            {
                let seek_to = SeekTo::Time { time: Time::from(seek_ts), track_id: Some(track_id) };
                match reader.seek(SeekMode::Accurate, seek_to)
                {
                    Ok(seeked_to) => seeked_to.required_ts,
                    Err(_) => 0
                }
            } else { 0 };

            if SEEK_TS.read().unwrap().is_some()
            {
                *SEEK_TS.write().unwrap() = None;
                decoder.reset();
            }

            // Decode the next packet.
            let packet = match reader.next_packet()
            {
                Ok(packet) => packet,
                Err(_err) => break
            };

            if packet.track_id() != track_id { continue; }

            match decoder.decode(&packet)
            {
                Err(err) => panic!("ERR: Failed to decode sound. {err}"),
                Ok(decoded) => {
                    if packet.ts() < seek_ts { continue; }
                    
                    // Update the progress stream with calculated times.
                    update_progress_state_stream(ProgressState {
                        position: timebase.calc_time(packet.ts()).seconds,
                        duration: timebase.calc_time(duration).seconds
                    });

                    // Write the decoded packet to CPAL.
                    if cpal_output.is_none()
                    {
                        let spec = *decoded.spec();
                        let duration = decoded.capacity() as u64;
                        cpal_output.replace(CpalOutput::build_stream(spec, duration));
                    }

                    cpal_output.as_mut().unwrap().write(decoded);
                }
            }
        }

        // Fix race condition.
        // If this gets called in `thread::spawn` in Player::open,
        // the playback state stream will produce false instead of true.
        // Calling it here makes it so that it is set to false before it is
        // set to true.
        crate::Player::internal_pause();
    }
}