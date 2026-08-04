use std::{
    cell::RefCell,
    rc::Rc,
    thread,
    time::{Duration, Instant},
};

use anyhow::{anyhow, bail, Context as _, Result};
use pulse::{
    callbacks::ListResult,
    context::{
        introspect::SinkInfo,
        Context, FlagSet as ContextFlagSet, State as ContextState,
    },
    mainloop::standard::{IterateResult, Mainloop},
};

use crate::hid::HeadsetState;

const HYPERX_VENDOR_ID: u16 = 0x03f0;
const CLOUD_III_S_PRODUCT_IDS: [u16; 2] = [0x06be, 0x02cc];
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const OPERATION_TIMEOUT: Duration = Duration::from_secs(3);
const MAINLOOP_POLL_DELAY: Duration = Duration::from_millis(1);

const VENDOR_ID_KEYS: [&str; 2] = ["device.vendor.id", "alsa.vendor_id"];
const PRODUCT_ID_KEYS: [&str; 2] = ["device.product.id", "alsa.product_id"];
const SERIAL_KEYS: [&str; 3] = ["device.serial", "device.string", "alsa.long_card_name"];
const PRODUCT_NAME_KEYS: [&str; 3] = [
    "device.product.name",
    "device.description",
    "alsa.card_name",
];

pub(crate) struct PulseRouter {
    connection: PulseConnection,
    remembered_speaker: Option<RememberedSink>,
}

#[derive(Debug)]
pub(crate) struct ReconcileResult {
    pub(crate) target_name: String,
    pub(crate) default_changed: bool,
    pub(crate) moved_streams: usize,
    pub(crate) failed_stream_moves: usize,
}

impl PulseRouter {
    pub(crate) fn connect() -> Result<Self> {
        Ok(Self {
            connection: PulseConnection::connect()?,
            remembered_speaker: None,
        })
    }

    pub(crate) fn reconnect(&mut self) -> Result<()> {
        // Replace only the transport so the remembered desktop sink survives
        // a pipewire-pulse restart.
        let connection = PulseConnection::connect()?;
        self.connection = connection;
        Ok(())
    }

    pub(crate) fn is_healthy(&self) -> bool {
        self.connection.is_ready()
    }

    pub(crate) fn reconcile(
        &mut self,
        state: HeadsetState,
        speaker_override: Option<&str>,
    ) -> Result<ReconcileResult> {
        let sinks = self.connection.list_sinks()?;
        let current_default = self.connection.default_sink_name()?;
        let current_default_sink = current_default
            .as_deref()
            .and_then(|name| find_sink_by_name(&sinks, name));

        let target = if state.is_connected() {
            if let Some(sink) = current_default_sink.filter(|sink| !is_hyperx_sink(sink)) {
                self.remembered_speaker = Some(RememberedSink::from_sink(sink));
            }

            find_hyperx_sink(&sinks)
                .cloned()
                .ok_or_else(|| {
                    anyhow!("headset is connected, but the HyperX playback sink is unavailable")
                })?
        } else {
            let target = resolve_speaker_sink(
                &sinks,
                current_default_sink,
                speaker_override,
                self.remembered_speaker.as_ref(),
            )?
            .clone();

            self.remembered_speaker = Some(RememberedSink::from_sink(&target));
            target
        };

        let mut default_changed = false;
        if current_default.as_deref() != Some(target.name.as_str()) {
            if !self.connection.set_default_sink(&target.name)? {
                bail!("PulseAudio refused to set default sink to {}", target.name);
            }
            default_changed = true;
        }

        let sink_inputs = self.connection.list_sink_inputs()?;
        let mut moved_streams = 0usize;
        let mut failed_stream_moves = 0usize;

        for input in sink_inputs {
            if input.sink_index == target.index {
                continue;
            }

            match self.connection.move_sink_input(input.index, target.index) {
                Ok(true) => moved_streams += 1,
                Ok(false) => failed_stream_moves += 1,
                Err(error) => {
                    if !self.connection.is_ready() {
                        return Err(error);
                    }
                    // A playback stream can disappear between the list and move
                    // operations. Keep routing the remaining streams.
                    failed_stream_moves += 1;
                }
            }
        }

        Ok(ReconcileResult {
            target_name: target.name,
            default_changed,
            moved_streams,
            failed_stream_moves,
        })
    }
}

struct PulseConnection {
    // Context must be dropped before the mainloop it references.
    context: Context,
    mainloop: Mainloop,
}

impl PulseConnection {
    fn connect() -> Result<Self> {
        let mainloop = Mainloop::new()
            .ok_or_else(|| anyhow!("failed to create PulseAudio mainloop"))?;
        let mut context = Context::new(&mainloop, "hyperx-audio-switchd")
            .ok_or_else(|| anyhow!("failed to create PulseAudio context"))?;

        context
            .connect(None, ContextFlagSet::NOAUTOSPAWN, None)
            .context("failed to start PulseAudio/PipeWire connection")?;

        let mut connection = Self { context, mainloop };
        connection.wait_until_ready()?;
        Ok(connection)
    }

    fn is_ready(&self) -> bool {
        matches!(self.context.get_state(), ContextState::Ready)
    }

    fn wait_until_ready(&mut self) -> Result<()> {
        let deadline = Instant::now() + CONNECT_TIMEOUT;

        loop {
            match self.context.get_state() {
                ContextState::Ready => return Ok(()),
                ContextState::Failed => bail!("PulseAudio/PipeWire connection failed"),
                ContextState::Terminated => bail!("PulseAudio/PipeWire connection terminated"),
                _ => {}
            }

            if Instant::now() >= deadline {
                bail!("timed out connecting to PulseAudio/PipeWire");
            }

            self.iterate_nonblocking()?;
            thread::sleep(MAINLOOP_POLL_DELAY);
        }
    }

    fn iterate_nonblocking(&mut self) -> Result<()> {
        match self.mainloop.iterate(false) {
            IterateResult::Success(_) => {}
            IterateResult::Quit(value) => {
                bail!("PulseAudio mainloop quit unexpectedly with {value:?}")
            }
            IterateResult::Err(error) => bail!("PulseAudio mainloop error: {error}"),
        }

        match self.context.get_state() {
            ContextState::Failed => bail!("PulseAudio/PipeWire connection failed"),
            ContextState::Terminated => bail!("PulseAudio/PipeWire connection terminated"),
            _ => Ok(()),
        }
    }

    fn wait_for<T>(&mut self, slot: &SharedResult<T>, operation: &str) -> Result<T> {
        let deadline = Instant::now() + OPERATION_TIMEOUT;

        loop {
            if let Some(result) = slot.borrow_mut().take() {
                return result.map_err(|message| anyhow!(message));
            }

            if Instant::now() >= deadline {
                bail!("timed out while {operation}");
            }

            self.iterate_nonblocking()?;
            thread::sleep(MAINLOOP_POLL_DELAY);
        }
    }

    fn list_sinks(&mut self) -> Result<Vec<Sink>> {
        let items = Rc::new(RefCell::new(Vec::new()));
        let result = shared_result();
        let callback_items = Rc::clone(&items);
        let callback_result = Rc::clone(&result);

        let operation = self
            .context
            .introspect()
            .get_sink_info_list(move |entry| match entry {
                ListResult::Item(info) => {
                    if let Some(sink) = Sink::from_pulse(info) {
                        callback_items.borrow_mut().push(sink);
                    }
                }
                ListResult::End => {
                    let sinks = std::mem::take(&mut *callback_items.borrow_mut());
                    *callback_result.borrow_mut() = Some(Ok(sinks));
                }
                ListResult::Error => {
                    *callback_result.borrow_mut() =
                        Some(Err("PulseAudio failed while listing sinks".to_owned()));
                }
            });

        let value = self.wait_for(&result, "listing playback sinks");
        drop(operation);
        value
    }

    fn default_sink_name(&mut self) -> Result<Option<String>> {
        let result = shared_result();
        let callback_result = Rc::clone(&result);

        let operation = self
            .context
            .introspect()
            .get_server_info(move |info| {
                let name = info.default_sink_name.as_ref().map(ToString::to_string);
                *callback_result.borrow_mut() = Some(Ok(name));
            });

        let value = self.wait_for(&result, "reading the default sink");
        drop(operation);
        value
    }

    fn set_default_sink(&mut self, name: &str) -> Result<bool> {
        let result = shared_result();
        let callback_result = Rc::clone(&result);
        let operation = self.context.set_default_sink(name, move |success| {
            *callback_result.borrow_mut() = Some(Ok(success));
        });

        let value = self.wait_for(&result, "setting the default sink");
        drop(operation);
        value
    }

    fn list_sink_inputs(&mut self) -> Result<Vec<SinkInput>> {
        let items = Rc::new(RefCell::new(Vec::new()));
        let result = shared_result();
        let callback_items = Rc::clone(&items);
        let callback_result = Rc::clone(&result);

        let operation = self
            .context
            .introspect()
            .get_sink_input_info_list(move |entry| match entry {
                ListResult::Item(info) => callback_items.borrow_mut().push(SinkInput {
                    index: info.index,
                    sink_index: info.sink,
                }),
                ListResult::End => {
                    let inputs = std::mem::take(&mut *callback_items.borrow_mut());
                    *callback_result.borrow_mut() = Some(Ok(inputs));
                }
                ListResult::Error => {
                    *callback_result.borrow_mut() =
                        Some(Err("PulseAudio failed while listing playback streams".to_owned()));
                }
            });

        let value = self.wait_for(&result, "listing playback streams");
        drop(operation);
        value
    }

    fn move_sink_input(&mut self, input_index: u32, sink_index: u32) -> Result<bool> {
        let result = shared_result();
        let callback_result = Rc::clone(&result);
        let mut introspector = self.context.introspect();
        let operation = introspector.move_sink_input_by_index(
            input_index,
            sink_index,
            Some(Box::new(move |success| {
                *callback_result.borrow_mut() = Some(Ok(success));
            })),
        );

        let value = self.wait_for(&result, "moving a playback stream");
        drop(operation);
        value
    }
}

type SharedResult<T> = Rc<RefCell<Option<std::result::Result<T, String>>>>;

fn shared_result<T>() -> SharedResult<T> {
    Rc::new(RefCell::new(None))
}

#[derive(Debug, Clone)]
struct Sink {
    name: String,
    index: u32,
    description: String,
    driver: String,
    vendor_id: Option<String>,
    product_id: Option<String>,
    serial: Option<String>,
    product_name: Option<String>,
    bus: Option<String>,
}

impl Sink {
    fn from_pulse(info: &SinkInfo<'_>) -> Option<Self> {
        Some(Self {
            name: info.name.as_ref()?.to_string(),
            index: info.index,
            description: info
                .description
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_default(),
            driver: info
                .driver
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_default(),
            vendor_id: first_property(info, &VENDOR_ID_KEYS),
            product_id: first_property(info, &PRODUCT_ID_KEYS),
            serial: first_property(info, &SERIAL_KEYS),
            product_name: first_property(info, &PRODUCT_NAME_KEYS),
            bus: info.proplist.get_str("device.bus"),
        })
    }
}

#[derive(Debug, Clone)]
struct SinkInput {
    index: u32,
    sink_index: u32,
}

#[derive(Debug, Clone)]
struct RememberedSink {
    name: String,
    serial: Option<String>,
    vendor_id: Option<String>,
    product_id: Option<String>,
    product_name: Option<String>,
    description: String,
}

impl RememberedSink {
    fn from_sink(sink: &Sink) -> Self {
        Self {
            name: sink.name.clone(),
            serial: sink.serial.clone(),
            vendor_id: sink.vendor_id.clone(),
            product_id: sink.product_id.clone(),
            product_name: sink.product_name.clone(),
            description: sink.description.clone(),
        }
    }

    fn match_score(&self, sink: &Sink) -> Option<u16> {
        if self.name == sink.name {
            return Some(1_000);
        }

        if nonempty_equal(self.serial.as_deref(), sink.serial.as_deref()) {
            return Some(900);
        }

        let hardware_matches = nonempty_equal(self.vendor_id.as_deref(), sink.vendor_id.as_deref())
            && nonempty_equal(self.product_id.as_deref(), sink.product_id.as_deref());
        let product_matches = optional_normalized_equal(
            self.product_name.as_deref(),
            sink.product_name.as_deref(),
        );
        let description_matches = normalized_equal(&self.description, &sink.description);

        if hardware_matches && (product_matches || description_matches) {
            return Some(800);
        }
        if product_matches {
            return Some(500);
        }
        if description_matches {
            return Some(300);
        }

        None
    }
}

fn resolve_speaker_sink<'a>(
    sinks: &'a [Sink],
    current_default: Option<&'a Sink>,
    speaker_override: Option<&str>,
    remembered_speaker: Option<&RememberedSink>,
) -> Result<&'a Sink> {
    if let Some(name) = speaker_override {
        return find_sink_by_name(sinks, name)
            .ok_or_else(|| anyhow!("configured speaker sink is unavailable: {name}"));
    }

    // While the headset is off, respect a non-HyperX output selected by the user.
    if let Some(sink) = current_default.filter(|sink| !is_hyperx_sink(sink)) {
        return Ok(sink);
    }

    if let Some(remembered) = remembered_speaker {
        if let Some(sink) = find_remembered_sink(sinks, remembered) {
            return Ok(sink);
        }
    }

    find_best_fallback_sink(sinks).ok_or_else(|| {
        anyhow!(
            "no usable non-HyperX playback sink found; select the speakers once or pass --speaker-sink"
        )
    })
}

fn find_remembered_sink<'a>(
    sinks: &'a [Sink],
    remembered: &RememberedSink,
) -> Option<&'a Sink> {
    let mut best: Option<(u16, &Sink)> = None;
    let mut best_is_ambiguous = false;

    for sink in sinks.iter().filter(|sink| !is_hyperx_sink(sink)) {
        let Some(score) = remembered.match_score(sink) else {
            continue;
        };

        match best {
            None => {
                best = Some((score, sink));
                best_is_ambiguous = false;
            }
            Some((best_score, _)) if score > best_score => {
                best = Some((score, sink));
                best_is_ambiguous = false;
            }
            Some((best_score, _)) if score == best_score => {
                best_is_ambiguous = true;
            }
            Some(_) => {}
        }
    }

    if best_is_ambiguous {
        None
    } else {
        best.map(|(_, sink)| sink)
    }
}

fn find_hyperx_sink(sinks: &[Sink]) -> Option<&Sink> {
    sinks.iter().find(|sink| is_hyperx_sink(sink))
}

fn find_sink_by_name<'a>(sinks: &'a [Sink], name: &str) -> Option<&'a Sink> {
    sinks.iter().find(|sink| sink.name == name)
}

fn is_hyperx_sink(sink: &Sink) -> bool {
    let ids_match = sink
        .vendor_id
        .as_deref()
        .is_some_and(|value| id_value_matches(value, HYPERX_VENDOR_ID))
        && sink.product_id.as_deref().is_some_and(|value| {
            CLOUD_III_S_PRODUCT_IDS
                .iter()
                .any(|expected| id_value_matches(value, *expected))
        });

    ids_match
        || is_hyperx_name_fallback(
            &sink.name,
            &sink.description,
            sink.product_name.as_deref(),
        )
}

fn is_hyperx_name_fallback(name: &str, description: &str, product_name: Option<&str>) -> bool {
    let text = normalize_text(&format!(
        "{name} {description} {}",
        product_name.unwrap_or_default()
    ));
    text.contains("hyperx") && text.contains("cloud iii s") && text.contains("wireless")
}

fn find_best_fallback_sink(sinks: &[Sink]) -> Option<&Sink> {
    sinks
        .iter()
        .filter(|sink| !is_hyperx_sink(sink))
        .filter_map(|sink| speaker_score(sink).map(|score| (score, sink)))
        .max_by_key(|(score, _)| *score)
        .map(|(_, sink)| sink)
}

fn speaker_score(sink: &Sink) -> Option<i32> {
    let text = normalize_text(&format!(
        "{} {} {} {} {}",
        sink.name,
        sink.description,
        sink.driver,
        sink.product_name.as_deref().unwrap_or_default(),
        sink.bus.as_deref().unwrap_or_default()
    ));

    if text.contains("null")
        || text.contains("dummy")
        || text.contains("hdmi")
        || text.contains("displayport")
        || text.contains("bluez")
        || text.contains("bluetooth")
        || text.contains("raop")
        || text.contains("network")
    {
        return None;
    }

    let mut score = 0;
    if text.contains("alsa output") {
        score += 20;
    }
    if text.contains("usb") {
        score += 100;
    }
    if text.contains("speaker") || text.contains("speakers") {
        score += 50;
    }
    if text.contains("analog") {
        score += 30;
    }
    if text.contains("pro output") {
        score += 20;
    }

    Some(score)
}

fn first_property(info: &SinkInfo<'_>, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| info.proplist.get_str(key))
}

fn id_value_matches(raw: &str, expected: u16) -> bool {
    let normalized = raw.trim().to_ascii_lowercase();
    let suffix = normalized.rsplit(':').next().unwrap_or(normalized.as_str());
    let suffix = suffix.strip_prefix("0x").unwrap_or(suffix);

    u16::from_str_radix(suffix, 16).is_ok_and(|value| value == expected)
        || suffix.parse::<u16>().is_ok_and(|value| value == expected)
}

fn nonempty_equal(left: Option<&str>, right: Option<&str>) -> bool {
    matches!((left, right), (Some(left), Some(right)) if !left.is_empty() && left == right)
}

fn optional_normalized_equal(left: Option<&str>, right: Option<&str>) -> bool {
    matches!((left, right), (Some(left), Some(right)) if normalized_equal(left, right))
}

fn normalized_equal(left: &str, right: &str) -> bool {
    let left = normalize_text(left);
    !left.is_empty() && left == normalize_text(right)
}

fn normalize_text(raw: &str) -> String {
    let normalized: String = raw
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                ' '
            }
        })
        .collect();

    normalized.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::{
        find_remembered_sink, id_value_matches, is_hyperx_sink, speaker_score, RememberedSink,
        Sink, CLOUD_III_S_PRODUCT_IDS, HYPERX_VENDOR_ID,
    };

    fn sink(name: &str, description: &str) -> Sink {
        Sink {
            name: name.to_owned(),
            index: 1,
            description: description.to_owned(),
            driver: String::new(),
            vendor_id: None,
            product_id: None,
            serial: None,
            product_name: None,
            bus: None,
        }
    }

    #[test]
    fn matches_hyperx_by_usb_properties() {
        let mut device = sink("generated-name", "Unknown USB Audio");
        device.vendor_id = Some("usb:03f0".to_owned());
        device.product_id = Some("0x06be".to_owned());
        assert!(is_hyperx_sink(&device));
    }

    #[test]
    fn matches_pipewire_style_hyperx_name_as_fallback() {
        let device = sink(
            "alsa_output.usb-HP_Inc._HyperX_Cloud_III_S_Wireless-00.analog-stereo",
            "HyperX Cloud III S Wireless Analog Stereo",
        );
        assert!(is_hyperx_sink(&device));
    }

    #[test]
    fn rejects_other_hyperx_models() {
        let device = sink(
            "alsa_output.usb-HP_HyperX_Cloud_III_Wireless-00.analog-stereo",
            "HyperX Cloud III Wireless",
        );
        assert!(!is_hyperx_sink(&device));
    }

    #[test]
    fn parses_common_usb_id_formats() {
        assert!(id_value_matches("usb:03f0", HYPERX_VENDOR_ID));
        assert!(id_value_matches("0x03f0", HYPERX_VENDOR_ID));
        assert!(id_value_matches("03f0", HYPERX_VENDOR_ID));
        assert!(id_value_matches("1008", HYPERX_VENDOR_ID));
        assert!(id_value_matches("usb:06be", CLOUD_III_S_PRODUCT_IDS[0]));
    }

    #[test]
    fn remembers_a_device_across_generated_name_changes() {
        let mut old = sink("old.generated.name", "Generic USB Audio");
        old.serial = Some("usb-1234".to_owned());
        let remembered = RememberedSink::from_sink(&old);

        let mut new = sink("new.generated.name", "Generic USB Audio");
        new.serial = Some("usb-1234".to_owned());
        assert_eq!(
            find_remembered_sink(&[new], &remembered).map(|sink| sink.name.as_str()),
            Some("new.generated.name")
        );
    }

    #[test]
    fn prefers_usb_speakers_over_generic_analog() {
        let mut usb = sink(
            "alsa_output.usb-Generic_USB_Audio-00.pro-output-2",
            "Generic USB Audio Pro Output 2",
        );
        usb.bus = Some("usb".to_owned());
        let analog = sink(
            "alsa_output.pci-0000_00_1f.3.analog-stereo",
            "Built-in Audio Analog Stereo",
        );
        assert!(speaker_score(&usb).unwrap() > speaker_score(&analog).unwrap());
    }

    #[test]
    fn ambiguous_description_does_not_guess_a_remembered_device() {
        let old = sink("old.generated.name", "Generic USB Audio");
        let remembered = RememberedSink::from_sink(&old);
        let first = sink("first.generated.name", "Generic USB Audio");
        let second = sink("second.generated.name", "Generic USB Audio");

        assert!(find_remembered_sink(&[first, second], &remembered).is_none());
    }

    #[test]
    fn usb_id_must_match_the_cloud_iii_s() {
        let mut device = sink("generated-name", "Unknown USB Audio");
        device.vendor_id = Some("03f0".to_owned());
        device.product_id = Some("1234".to_owned());
        assert!(!is_hyperx_sink(&device));
    }

    #[test]
    fn excludes_hdmi_and_virtual_outputs() {
        assert_eq!(
            speaker_score(&sink("alsa_output.pci.example.hdmi-stereo", "HDMI Output")),
            None
        );
        assert_eq!(speaker_score(&sink("auto_null", "Dummy Output")), None);
    }
}
