mod audio;
mod hid;

use std::{
    num::NonZeroU64,
    thread,
    time::{Duration, Instant},
};

use anyhow::{bail, Result};
use clap::Parser;

use audio::PulseRouter;
use hid::{HeadsetProbe, HeadsetState};

const HID_ERROR_RETRY_DELAY: Duration = Duration::from_secs(2);
const AUDIO_ERROR_RETRY_DELAY: Duration = Duration::from_secs(1);
const HID_FAILURES_BEFORE_DISCONNECT: u8 = 2;

#[derive(Debug, Parser)]
#[command(version, about)]
struct Args {
    /// Optional exact PipeWire/PulseAudio sink name for the desktop speakers.
    /// Normally the current non-HyperX output is detected and remembered.
    #[arg(long)]
    speaker_sink: Option<String>,

    /// HyperX HID polling interval in milliseconds.
    #[arg(long, default_value = "250")]
    poll_ms: NonZeroU64,

    /// Periodic audio routing verification interval in seconds.
    /// Audio is also reconciled immediately whenever the headset state changes.
    #[arg(long, default_value = "15")]
    audio_verify_secs: NonZeroU64,

    /// Query and reconcile once, then exit.
    #[arg(long)]
    once: bool,

    /// Print unchanged reconciliation state too.
    #[arg(short, long)]
    verbose: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();
    if args
        .speaker_sink
        .as_deref()
        .is_some_and(|name| name.trim().is_empty())
    {
        bail!("--speaker-sink must not be empty");
    }

    run(args)
}

fn run(args: Args) -> Result<()> {
    let poll_interval = Duration::from_millis(args.poll_ms.get());
    let audio_verify_interval = Duration::from_secs(args.audio_verify_secs.get());
    let mut headset = HeadsetProbe::default();
    let mut pulse: Option<PulseRouter> = None;
    let mut last_reported_headset_state: Option<HeadsetState> = None;
    let mut routing_state: Option<HeadsetState> = None;
    let mut reconcile_pending = true;
    let mut next_hid_query = Instant::now();
    let mut next_audio_attempt = Instant::now();
    let mut next_audio_verification = Instant::now();
    let mut last_target: Option<String> = None;
    let mut last_hid_error: Option<String> = None;
    let mut consecutive_hid_failures = 0u8;
    let mut last_pulse_error: Option<String> = None;

    loop {
        let cycle_started = Instant::now();

        let observed_routing_state = if cycle_started < next_hid_query {
            routing_state.unwrap_or(HeadsetState::Disconnected)
        } else {
            match headset.query() {
                Ok(state) => {
                    last_hid_error = None;
                    consecutive_hid_failures = 0;
                    next_hid_query = cycle_started;
                    if last_reported_headset_state != Some(state) {
                        eprintln!("headset state: {state}");
                        last_reported_headset_state = Some(state);
                    }
                    state
                }
                Err(error) => {
                    report_distinct_error("hid error", &error, &mut last_hid_error);
                    if args.once {
                        return Err(error);
                    }

                    next_hid_query = cycle_started + HID_ERROR_RETRY_DELAY;
                    last_reported_headset_state = None;
                    consecutive_hid_failures = consecutive_hid_failures.saturating_add(1);

                    // Avoid rerouting audio after one transient USB failure. A
                    // missing or inaccessible dongle still falls back shortly
                    // afterwards and cannot leave a vanished HyperX sink active.
                    if consecutive_hid_failures >= HID_FAILURES_BEFORE_DISCONNECT {
                        HeadsetState::Disconnected
                    } else {
                        routing_state.unwrap_or(HeadsetState::Disconnected)
                    }
                }
            }
        };

        if routing_state != Some(observed_routing_state) {
            routing_state = Some(observed_routing_state);
            reconcile_pending = true;
            next_audio_attempt = cycle_started;
        }

        if Instant::now() >= next_audio_verification {
            reconcile_pending = true;
        }

        if reconcile_pending && cycle_started >= next_audio_attempt {
            let mut audio_ready = true;

            if let Some(router) = pulse.as_mut() {
                if !router.is_healthy() {
                    if let Err(error) = router.reconnect() {
                        report_distinct_error("audio error", &error, &mut last_pulse_error);
                        next_audio_attempt = Instant::now() + AUDIO_ERROR_RETRY_DELAY;
                        audio_ready = false;
                        if args.once {
                            return Err(error);
                        }
                    }
                }
            } else {
                match PulseRouter::connect() {
                    Ok(router) => pulse = Some(router),
                    Err(error) => {
                        report_distinct_error("audio error", &error, &mut last_pulse_error);
                        next_audio_attempt = Instant::now() + AUDIO_ERROR_RETRY_DELAY;
                        audio_ready = false;
                        if args.once {
                            return Err(error);
                        }
                    }
                }
            }

            if audio_ready {
                if let Some(router) = pulse.as_mut() {
                    match router.reconcile(observed_routing_state, args.speaker_sink.as_deref()) {
                        Ok(result) => {
                            last_pulse_error = None;
                            let target_changed =
                                last_target.as_deref() != Some(result.target_name.as_str());

                            if target_changed
                                || result.default_changed
                                || result.moved_streams > 0
                                || result.failed_stream_moves > 0
                                || args.verbose
                            {
                                eprintln!(
                                    "audio target: {} (default_changed={}, moved_streams={}, failed_stream_moves={})",
                                    result.target_name,
                                    result.default_changed,
                                    result.moved_streams,
                                    result.failed_stream_moves
                                );
                            }

                            last_target = Some(result.target_name);
                            reconcile_pending = false;
                            next_audio_attempt = cycle_started;
                            next_audio_verification = Instant::now() + audio_verify_interval;
                        }
                        Err(error) => {
                            report_distinct_error("audio error", &error, &mut last_pulse_error);
                            next_audio_attempt = Instant::now() + AUDIO_ERROR_RETRY_DELAY;
                            if args.once {
                                return Err(error);
                            }
                        }
                    }
                }
            }
        }

        if args.once {
            return Ok(());
        }

        thread::sleep(poll_interval.saturating_sub(cycle_started.elapsed()));
    }
}

fn report_distinct_error(prefix: &str, error: &anyhow::Error, previous: &mut Option<String>) {
    let message = format!("{error:#}");
    if previous.as_deref() != Some(message.as_str()) {
        eprintln!("{prefix}: {message}");
        *previous = Some(message);
    }
}
