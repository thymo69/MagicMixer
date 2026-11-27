//! In-process VST3 host: load a plugin, run its audio, feed it MIDI and
//! parameter changes, save/restore its state, and open its editor window.
//!
//! Threading (per the VST3 spec): load/unload, state, and the editor happen on
//! the UI thread; `Plugin::process` runs on a rack's audio thread.

use std::collections::{HashMap, HashSet};
use std::ffi::{c_char, c_void, CStr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use vst3::Steinberg::Vst::*;
use vst3::Steinberg::*;
use vst3::{Class, ComPtr, ComRef, ComWrapper, Interface};

/// Frames per processing block (5.3 ms at 48 kHz).
pub const BLOCK: usize = 256;
pub const SAMPLE_RATE: f64 = 48_000.0;

/// Which plugin to load (from the scan).
#[derive(Debug, Clone, PartialEq)]
pub struct PluginDesc {
    pub path: PathBuf,
    pub name: String,
    /// "Instrument" or "Effect".
    pub kind: String,
}

impl PluginDesc {
    pub fn is_instrument(&self) -> bool {
        self.kind == "Instrument"
    }
}

/// Saved plugin state: (component/processor state, controller state).
pub type PluginState = (Vec<u8>, Vec<u8>);

// ===========================================================================
// Host-side COM objects
// ===========================================================================

struct HostApp;
impl Class for HostApp {
    type Interfaces = (IHostApplication,);
}
impl IHostApplicationTrait for HostApp {
    unsafe fn getName(&self, name: *mut String128) -> tresult {
        let n = &mut *name;
        for (i, c) in "Magic Mixer".encode_utf16().chain(std::iter::once(0)).enumerate() {
            n[i] = c as TChar;
        }
        kResultOk
    }
    unsafe fn createInstance(&self, cid: *mut TUID, iid: *mut TUID, obj: *mut *mut c_void) -> tresult {
        let cid = *(cid as *const [u8; 16]);
        let iid = *(iid as *const [u8; 16]);
        if cid == IMessage::IID && iid == IMessage::IID {
            let m = ComWrapper::new(HostMessage::default());
            *obj = m.to_com_ptr::<IMessage>().unwrap().into_raw() as *mut c_void;
            return kResultOk;
        }
        if cid == IAttributeList::IID && iid == IAttributeList::IID {
            let a = ComWrapper::new(Attributes::default());
            *obj = a.to_com_ptr::<IAttributeList>().unwrap().into_raw() as *mut c_void;
            return kResultOk;
        }
        *obj = std::ptr::null_mut();
        kResultFalse
    }
}

fn host_unknown() -> *mut FUnknown {
    static HOST: OnceLock<ComWrapper<HostApp>> = OnceLock::new();
    HOST.get_or_init(|| ComWrapper::new(HostApp)).as_com_ref::<IHostApplication>().unwrap().as_ptr() as *mut FUnknown
}

enum AttrVal {
    Int(i64),
    Float(f64),
    Str(Vec<u16>),
    Bin(Vec<u8>),
}

#[derive(Default)]
struct Attributes {
    map: Mutex<HashMap<String, AttrVal>>,
}
impl Class for Attributes {
    type Interfaces = (IAttributeList,);
}
type AttrID = *const c_char;
unsafe fn key(id: AttrID) -> String {
    CStr::from_ptr(id).to_string_lossy().into_owned()
}
impl IAttributeListTrait for Attributes {
    unsafe fn setInt(&self, id: AttrID, value: int64) -> tresult {
        self.map.lock().unwrap().insert(key(id), AttrVal::Int(value));
        kResultOk
    }
    unsafe fn getInt(&self, id: AttrID, value: *mut int64) -> tresult {
        match self.map.lock().unwrap().get(&key(id)) {
            Some(AttrVal::Int(v)) => {
                *value = *v;
                kResultOk
            }
            _ => kResultFalse,
        }
    }
    unsafe fn setFloat(&self, id: AttrID, value: f64) -> tresult {
        self.map.lock().unwrap().insert(key(id), AttrVal::Float(value));
        kResultOk
    }
    unsafe fn getFloat(&self, id: AttrID, value: *mut f64) -> tresult {
        match self.map.lock().unwrap().get(&key(id)) {
            Some(AttrVal::Float(v)) => {
                *value = *v;
                kResultOk
            }
            _ => kResultFalse,
        }
    }
    unsafe fn setString(&self, id: AttrID, string: *const TChar) -> tresult {
        if string.is_null() {
            return kResultFalse;
        }
        let mut v = Vec::new();
        let mut p = string;
        while *p != 0 {
            v.push(*p as u16);
            p = p.add(1);
        }
        self.map.lock().unwrap().insert(key(id), AttrVal::Str(v));
        kResultOk
    }
    unsafe fn getString(&self, id: AttrID, string: *mut TChar, size_in_bytes: uint32) -> tresult {
        match self.map.lock().unwrap().get(&key(id)) {
            Some(AttrVal::Str(_)) if string.is_null() || size_in_bytes < 2 => kResultFalse,
            Some(AttrVal::Str(v)) => {
                let max = (size_in_bytes as usize / 2).saturating_sub(1);
                let n = v.len().min(max);
                for (i, c) in v[..n].iter().enumerate() {
                    *string.add(i) = *c as TChar;
                }
                *string.add(n) = 0;
                kResultOk
            }
            _ => kResultFalse,
        }
    }
    unsafe fn setBinary(&self, id: AttrID, data: *const c_void, size_in_bytes: uint32) -> tresult {
        let v = if data.is_null() || size_in_bytes == 0 {
            Vec::new()
        } else {
            std::slice::from_raw_parts(data as *const u8, size_in_bytes as usize).to_vec()
        };
        self.map.lock().unwrap().insert(key(id), AttrVal::Bin(v));
        kResultOk
    }
    unsafe fn getBinary(&self, id: AttrID, data: *mut *const c_void, size_in_bytes: *mut uint32) -> tresult {
        match self.map.lock().unwrap().get(&key(id)) {
            Some(AttrVal::Bin(v)) => {
                // The pointer stays valid while this attribute list lives.
                *data = v.as_ptr() as *const c_void;
                *size_in_bytes = v.len() as u32;
                kResultOk
            }
            _ => kResultFalse,
        }
    }
}

struct HostMessage {
    id: Mutex<std::ffi::CString>,
    attrs: ComWrapper<Attributes>,
}
impl Default for HostMessage {
    fn default() -> Self {
        HostMessage { id: Mutex::new(Default::default()), attrs: ComWrapper::new(Attributes::default()) }
    }
}
impl Class for HostMessage {
    type Interfaces = (IMessage,);
}
impl IMessageTrait for HostMessage {
    unsafe fn getMessageID(&self) -> FIDString {
        self.id.lock().unwrap().as_ptr()
    }
    unsafe fn setMessageID(&self, id: FIDString) {
        *self.id.lock().unwrap() = if id.is_null() { Default::default() } else { CStr::from_ptr(id).to_owned() };
    }
    unsafe fn getAttributes(&self) -> *mut IAttributeList {
        self.attrs.as_com_ref::<IAttributeList>().unwrap().as_ptr()
    }
}

/// In-memory IBStream for plugin state.
#[derive(Default)]
struct MemStream {
    inner: Mutex<(Vec<u8>, usize)>,
}
impl Class for MemStream {
    type Interfaces = (IBStream,);
}
impl MemStream {
    fn with(data: Vec<u8>) -> Self {
        MemStream { inner: Mutex::new((data, 0)) }
    }
}
impl IBStreamTrait for MemStream {
    unsafe fn read(&self, buffer: *mut c_void, num_bytes: int32, read: *mut int32) -> tresult {
        let mut g = self.inner.lock().unwrap();
        let (data, pos) = &mut *g;
        let n = (num_bytes.max(0) as usize).min(data.len().saturating_sub(*pos));
        if n > 0 && buffer.is_null() {
            return kResultFalse;
        }
        std::ptr::copy_nonoverlapping(data.as_ptr().add(*pos), buffer as *mut u8, n);
        *pos += n;
        if !read.is_null() {
            *read = n as int32;
        }
        kResultOk
    }
    unsafe fn write(&self, buffer: *mut c_void, num_bytes: int32, written: *mut int32) -> tresult {
        let mut g = self.inner.lock().unwrap();
        let (data, pos) = &mut *g;
        let n = num_bytes.max(0) as usize;
        if n > 0 && buffer.is_null() {
            return kResultFalse;
        }
        let end = *pos + n;
        if data.len() < end {
            data.resize(end, 0);
        }
        std::ptr::copy_nonoverlapping(buffer as *const u8, data.as_mut_ptr().add(*pos), n);
        *pos = end;
        if !written.is_null() {
            *written = n as int32;
        }
        kResultOk
    }
    unsafe fn seek(&self, pos: int64, mode: int32, result: *mut int64) -> tresult {
        let mut g = self.inner.lock().unwrap();
        let (data, p) = &mut *g;
        let base = match mode {
            0 => 0i64,
            1 => *p as i64,
            _ => data.len() as i64,
        };
        *p = (base + pos).clamp(0, data.len() as i64) as usize;
        if !result.is_null() {
            *result = *p as int64;
        }
        kResultOk
    }
    unsafe fn tell(&self, pos: *mut int64) -> tresult {
        if !pos.is_null() {
            *pos = self.inner.lock().unwrap().1 as int64;
        }
        kResultOk
    }
}

/// Receives parameter edits from the plugin's editor and queues them for
/// the audio thread.
struct ComponentHandler {
    pending: Arc<Mutex<Vec<(ParamID, f64)>>>,
}
impl Class for ComponentHandler {
    type Interfaces = (IComponentHandler,);
}
impl IComponentHandlerTrait for ComponentHandler {
    unsafe fn beginEdit(&self, _id: ParamID) -> tresult {
        kResultOk
    }
    unsafe fn performEdit(&self, id: ParamID, value: ParamValue) -> tresult {
        self.pending.lock().unwrap().push((id, value));
        kResultOk
    }
    unsafe fn endEdit(&self, _id: ParamID) -> tresult {
        kResultOk
    }
    unsafe fn restartComponent(&self, _flags: int32) -> tresult {
        kResultOk
    }
}

#[derive(Default)]
struct EventList {
    events: Mutex<Vec<Event>>,
}
impl Class for EventList {
    type Interfaces = (IEventList,);
}
impl IEventListTrait for EventList {
    unsafe fn getEventCount(&self) -> int32 {
        self.events.lock().unwrap().len() as int32
    }
    unsafe fn getEvent(&self, index: int32, e: *mut Event) -> tresult {
        match self.events.lock().unwrap().get(index as usize) {
            Some(ev) => {
                *e = *ev;
                kResultOk
            }
            None => kInvalidArgument,
        }
    }
    unsafe fn addEvent(&self, e: *mut Event) -> tresult {
        self.events.lock().unwrap().push(*e);
        kResultOk
    }
}

struct ParamQueue {
    id: ParamID,
    points: Mutex<Vec<(int32, ParamValue)>>,
}
impl Class for ParamQueue {
    type Interfaces = (IParamValueQueue,);
}
impl IParamValueQueueTrait for ParamQueue {
    unsafe fn getParameterId(&self) -> ParamID {
        self.id
    }
    unsafe fn getPointCount(&self) -> int32 {
        self.points.lock().unwrap().len() as int32
    }
    unsafe fn getPoint(&self, index: int32, offset: *mut int32, value: *mut ParamValue) -> tresult {
        match self.points.lock().unwrap().get(index as usize) {
            Some(&(o, v)) => {
                *offset = o;
                *value = v;
                kResultOk
            }
            None => kInvalidArgument,
        }
    }
    unsafe fn addPoint(&self, offset: int32, value: ParamValue, index: *mut int32) -> tresult {
        let mut p = self.points.lock().unwrap();
        p.push((offset, value));
        if !index.is_null() {
            *index = p.len() as int32 - 1;
        }
        kResultOk
    }
}

#[derive(Default)]
struct ParamChanges {
    queues: Mutex<Vec<ComWrapper<ParamQueue>>>,
}
impl Class for ParamChanges {
    type Interfaces = (IParameterChanges,);
}
impl ParamChanges {
    fn clear(&self) {
        self.queues.lock().unwrap().clear();
    }
    fn add(&self, id: ParamID, value: f64) {
        let mut q = self.queues.lock().unwrap();
        match q.iter().find(|x| x.id == id) {
            Some(x) => x.points.lock().unwrap().push((0, value)),
            None => q.push(ComWrapper::new(ParamQueue { id, points: Mutex::new(vec![(0, value)]) })),
        }
    }
}
impl IParameterChangesTrait for ParamChanges {
    unsafe fn getParameterCount(&self) -> int32 {
        self.queues.lock().unwrap().len() as int32
    }
    unsafe fn getParameterData(&self, index: int32) -> *mut IParamValueQueue {
        match self.queues.lock().unwrap().get(index as usize) {
            Some(q) => q.as_com_ref::<IParamValueQueue>().unwrap().as_ptr(),
            None => std::ptr::null_mut(),
        }
    }
    unsafe fn addParameterData(&self, id: *const ParamID, index: *mut int32) -> *mut IParamValueQueue {
        let id = *id;
        let mut q = self.queues.lock().unwrap();
        let i = match q.iter().position(|x| x.id == id) {
            Some(i) => i,
            None => {
                q.push(ComWrapper::new(ParamQueue { id, points: Mutex::new(Vec::new()) }));
                q.len() - 1
            }
        };
        if !index.is_null() {
            *index = i as int32;
        }
        q[i].as_com_ref::<IParamValueQueue>().unwrap().as_ptr()
    }
}

// ===========================================================================
// Module / factory cache (modules are never unloaded: many plugins crash on it)
// ===========================================================================

struct Module {
    factory: ComPtr<IPluginFactory>,
}
unsafe impl Send for Module {}

fn factory_for(path: &Path) -> Result<ComPtr<IPluginFactory>, String> {
    use windows::core::{s, HSTRING};
    use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryExW, LOAD_WITH_ALTERED_SEARCH_PATH};
    static MODULES: OnceLock<Mutex<HashMap<PathBuf, Module>>> = OnceLock::new();
    let mut map = MODULES.get_or_init(Default::default).lock().unwrap();
    if let Some(m) = map.get(path) {
        return Ok(m.factory.clone());
    }
    let bin = crate::plugins::binary_of(path);
    unsafe {
        let module = LoadLibraryExW(&HSTRING::from(bin.as_os_str()), None, LOAD_WITH_ALTERED_SEARCH_PATH)
            .map_err(|e| format!("can't load {}: {}", bin.display(), e.message()))?;
        if let Some(init) = GetProcAddress(module, s!("InitDll")) {
            let init: unsafe extern "system" fn() -> bool = std::mem::transmute(init);
            init();
        }
        let get = GetProcAddress(module, s!("GetPluginFactory")).ok_or("not a VST3 plugin")?;
        let get: unsafe extern "system" fn() -> *mut c_void = std::mem::transmute(get);
        let factory = ComPtr::from_raw(get() as *mut IPluginFactory).ok_or("plugin returned no factory")?;
        map.insert(path.to_path_buf(), Module { factory: factory.clone() });
        Ok(factory)
    }
}

fn trace(msg: &str) {
    if std::env::var_os("MM_VST_DEBUG").is_some() {
        eprintln!("[vst] {msg}");
    }
}

fn cstr_buf(buf: &[c_char]) -> String {
    let bytes: Vec<u8> = buf.iter().take_while(|&&c| c != 0).map(|&c| c as u8).collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

// ===========================================================================
// A loaded plugin
// ===========================================================================

struct Bus {
    channels: Vec<Vec<f32>>,
    ptrs: Vec<*mut f32>,
}
impl Bus {
    fn new(n: usize) -> Bus {
        let mut channels: Vec<Vec<f32>> = (0..n).map(|_| vec![0.0; BLOCK]).collect();
        let ptrs = channels.iter_mut().map(|c| c.as_mut_ptr()).collect();
        Bus { channels, ptrs }
    }
    fn buffers(&mut self) -> AudioBusBuffers {
        AudioBusBuffers {
            numChannels: self.channels.len() as int32,
            silenceFlags: 0,
            __field0: AudioBusBuffers__type0 { channelBuffers32: self.ptrs.as_mut_ptr() },
        }
    }
}

pub struct Plugin {
    pub desc: PluginDesc,
    component: ComPtr<IComponent>,
    processor: ComPtr<IAudioProcessor>,
    controller: Option<ComPtr<IEditController>>,
    separate_controller: bool,
    links: Option<(ComPtr<IConnectionPoint>, ComPtr<IConnectionPoint>)>,
    _handler: ComWrapper<ComponentHandler>,
    pending: Arc<Mutex<Vec<(ParamID, f64)>>>,
    /// MIDI CC (and 128 = aftertouch, 129 = pitch bend) -> parameter.
    midi_map: HashMap<u16, ParamID>,
    in_buses: Vec<Bus>,
    out_buses: Vec<Bus>,
    has_events: bool,
    events: ComWrapper<EventList>,
    params_in: ComWrapper<ParamChanges>,
    params_out: ComWrapper<ParamChanges>,
    context: ProcessContext,
    pub bypass: bool,
}

// Plugin objects move between the UI thread (load/unload) and one audio thread.
unsafe impl Send for Plugin {}

impl Plugin {
    /// Load and start a plugin. Call on the UI thread.
    pub fn load(desc: &PluginDesc, state: Option<&PluginState>) -> Result<Plugin, String> {
        unsafe { Self::load_inner(desc, state) }
    }

    unsafe fn load_inner(desc: &PluginDesc, state: Option<&PluginState>) -> Result<Plugin, String> {
        trace(&format!("load module {}", desc.path.display()));
        let factory = factory_for(&desc.path)?;
        let host = host_unknown();
        trace("module loaded");

        // Find the audio class with this name (or the first audio class).
        let mut cid: Option<TUID> = None;
        for i in 0..factory.countClasses() {
            let mut ci: PClassInfo = std::mem::zeroed();
            if factory.getClassInfo(i, &mut ci) != kResultOk {
                continue;
            }
            if cstr_buf(&ci.category) != "Audio Module Class" {
                continue;
            }
            let name = cstr_buf(&ci.name);
            if cid.is_none() || name == desc.name {
                cid = Some(ci.cid);
                if name == desc.name {
                    break;
                }
            }
        }
        let cid = cid.ok_or("no audio class in plugin")?;

        let mut obj: *mut c_void = std::ptr::null_mut();
        let r = factory.createInstance(cid.as_ptr() as FIDString, IComponent::IID.as_ptr() as FIDString, &mut obj);
        let component = ComPtr::from_raw(obj as *mut IComponent).filter(|_| r == kResultOk).ok_or("couldn't create plugin")?;
        trace("component created");
        if component.initialize(host) != kResultOk {
            return Err("plugin failed to initialize".into());
        }

        // Controller: the same object, or a separate one we must create and connect.
        let (controller, separate_controller) = match component.cast::<IEditController>() {
            Some(c) => (Some(c), false),
            None => {
                let mut ccid: TUID = [0; 16];
                if component.getControllerClassId(&mut ccid) == kResultOk {
                    let mut obj: *mut c_void = std::ptr::null_mut();
                    let r = factory.createInstance(ccid.as_ptr() as FIDString, IEditController::IID.as_ptr() as FIDString, &mut obj);
                    match ComPtr::from_raw(obj as *mut IEditController).filter(|_| r == kResultOk) {
                        Some(c) if c.initialize(host) == kResultOk => (Some(c), true),
                        _ => (None, false),
                    }
                } else {
                    (None, false)
                }
            }
        };
        trace(&format!("controller ready (separate: {separate_controller})"));
        let links = match (&controller, separate_controller) {
            (Some(c), true) => match (component.cast::<IConnectionPoint>(), c.cast::<IConnectionPoint>()) {
                (Some(a), Some(b)) => {
                    a.connect(b.as_ptr());
                    b.connect(a.as_ptr());
                    Some((a, b))
                }
                _ => None,
            },
            _ => None,
        };

        let pending: Arc<Mutex<Vec<(ParamID, f64)>>> = Arc::default();
        let handler = ComWrapper::new(ComponentHandler { pending: pending.clone() });
        if let Some(c) = &controller {
            c.setComponentHandler(handler.as_com_ref::<IComponentHandler>().unwrap().as_ptr());
        }

        // State: restore a saved one, then let the controller mirror the processor.
        if let Some((comp_state, ctrl_state)) = state {
            if !comp_state.is_empty() {
                let s = ComWrapper::new(MemStream::with(comp_state.clone()));
                component.setState(s.as_com_ref::<IBStream>().unwrap().as_ptr());
            }
            if let Some(c) = &controller {
                if !comp_state.is_empty() {
                    let s = ComWrapper::new(MemStream::with(comp_state.clone()));
                    c.setComponentState(s.as_com_ref::<IBStream>().unwrap().as_ptr());
                }
                if !ctrl_state.is_empty() {
                    let s = ComWrapper::new(MemStream::with(ctrl_state.clone()));
                    c.setState(s.as_com_ref::<IBStream>().unwrap().as_ptr());
                }
            }
        } else if let Some(c) = &controller {
            let s = ComWrapper::new(MemStream::default());
            if component.getState(s.as_com_ref::<IBStream>().unwrap().as_ptr()) == kResultOk {
                s.inner.lock().unwrap().1 = 0; // rewind before handing to the controller
                c.setComponentState(s.as_com_ref::<IBStream>().unwrap().as_ptr());
            }
        }

        trace("state synced");
        let processor = component.cast::<IAudioProcessor>().ok_or("plugin has no audio processor")?;

        // Buses: main in/out stereo; others left as the plugin wants.
        let audio = MediaTypes_::kAudio as MediaType;
        let (din, dout) = (BusDirections_::kInput as BusDirection, BusDirections_::kOutput as BusDirection);
        let n_in = component.getBusCount(audio, din).max(0) as usize;
        let n_out = component.getBusCount(audio, dout).max(0) as usize;
        let arrangements = |dir, n: usize| -> Vec<SpeakerArrangement> {
            (0..n)
                .map(|i| {
                    let mut a: SpeakerArrangement = 0;
                    processor.getBusArrangement(dir, i as int32, &mut a);
                    if i == 0 {
                        SpeakerArr::kStereo
                    } else {
                        a
                    }
                })
                .collect()
        };
        let mut ins = arrangements(din, n_in);
        let mut outs = arrangements(dout, n_out);
        processor.setBusArrangements(ins.as_mut_ptr(), n_in as int32, outs.as_mut_ptr(), n_out as int32);
        // Read back what the plugin actually accepted.
        let channel_counts = |dir, n: usize| -> Vec<usize> {
            (0..n)
                .map(|i| {
                    let mut a: SpeakerArrangement = 0;
                    processor.getBusArrangement(dir, i as int32, &mut a);
                    (a as u64).count_ones() as usize
                })
                .collect()
        };
        let in_buses = channel_counts(din, n_in).into_iter().map(Bus::new).collect();
        let out_buses = channel_counts(dout, n_out).into_iter().map(Bus::new).collect();
        if n_in > 0 {
            component.activateBus(audio, din, 0, 1);
        }
        if n_out > 0 {
            component.activateBus(audio, dout, 0, 1);
        }
        let ev = MediaTypes_::kEvent as MediaType;
        let has_events = component.getBusCount(ev, din) > 0;
        if has_events {
            component.activateBus(ev, din, 0, 1);
        }

        let mut setup = ProcessSetup {
            processMode: ProcessModes_::kRealtime as int32,
            symbolicSampleSize: SymbolicSampleSizes_::kSample32 as int32,
            maxSamplesPerBlock: BLOCK as int32,
            sampleRate: SAMPLE_RATE,
        };
        if processor.setupProcessing(&mut setup) != kResultOk {
            return Err("plugin rejected 48 kHz / 32-bit processing".into());
        }
        trace("processing set up");
        if component.setActive(1) != kResultOk {
            return Err("plugin failed to activate".into());
        }
        processor.setProcessing(1);
        trace("active");

        // Cache MIDI CC -> parameter mappings (the controller isn't audio-thread safe).
        let mut midi_map = HashMap::new();
        if let Some(mm) = controller.as_ref().and_then(|c| c.cast::<IMidiMapping>()) {
            for cc in 0..130i16 {
                let mut id: ParamID = 0;
                if mm.getMidiControllerAssignment(0, 0, cc, &mut id) == kResultTrue {
                    midi_map.insert(cc as u16, id);
                }
            }
        }

        let mut context: ProcessContext = std::mem::zeroed();
        context.sampleRate = SAMPLE_RATE;
        context.tempo = 120.0;
        context.timeSigNumerator = 4;
        context.timeSigDenominator = 4;
        context.state = (ProcessContext_::StatesAndFlags_::kTempoValid | ProcessContext_::StatesAndFlags_::kTimeSigValid) as u32;

        Ok(Plugin {
            desc: desc.clone(),
            component,
            processor,
            controller,
            separate_controller,
            links,
            _handler: handler,
            pending,
            midi_map,
            in_buses,
            out_buses,
            has_events,
            events: ComWrapper::new(EventList::default()),
            params_in: ComWrapper::new(ParamChanges::default()),
            params_out: ComWrapper::new(ParamChanges::default()),
            context,
            bypass: false,
        })
    }

    pub fn has_editor(&self) -> bool {
        self.controller.is_some()
    }

    /// The plugin's parameters with current values, for the generic editor
    /// shown when a plugin has no window of its own. UI thread.
    pub fn params(&self) -> Vec<ParamView> {
        let Some(c) = &self.controller else { return Vec::new() };
        let mut out = Vec::new();
        unsafe {
            let n = c.getParameterCount().min(256);
            for i in 0..n {
                let mut info: ParameterInfo = std::mem::zeroed();
                if c.getParameterInfo(i, &mut info) != kResultOk {
                    continue;
                }
                let flags = info.flags as i32;
                if flags & (ParameterInfo_::ParameterFlags_::kIsReadOnly as i32) != 0
                    || flags & (ParameterInfo_::ParameterFlags_::kIsHidden as i32) != 0
                    || flags & (ParameterInfo_::ParameterFlags_::kIsProgramChange as i32) != 0
                    || wstr(&info.title).starts_with("VST_ProgramChange")
                {
                    continue;
                }
                let value = c.getParamNormalized(info.id);
                let mut s: String128 = [0; 128];
                c.getParamStringByValue(info.id, value, &mut s);
                out.push(ParamView {
                    id: info.id,
                    title: wstr(&info.title),
                    units: wstr(&info.units),
                    value,
                    text: wstr(&s),
                    steps: info.stepCount,
                });
            }
        }
        out
    }

    /// Set a parameter (normalized 0..1) from the generic editor. UI thread.
    pub fn set_param(&self, id: u32, value: f64) {
        if let Some(c) = &self.controller {
            unsafe {
                c.setParamNormalized(id, value);
            }
        }
        self.pending.lock().unwrap().push((id, value));
    }
}

/// One parameter, for the generic editor.
#[derive(Debug, Clone)]
pub struct ParamView {
    pub id: u32,
    pub title: String,
    pub units: String,
    pub value: f64,
    pub text: String,
    pub steps: i32,
}

fn wstr(s: &[TChar]) -> String {
    let n = s.iter().position(|&c| c == 0).unwrap_or(s.len());
    String::from_utf16_lossy(&s[..n].iter().map(|&c| c as u16).collect::<Vec<_>>())
}

/// What a plugin does with sound, measured by actually running it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SoundRole {
    /// Makes sound from MIDI (an instrument).
    Makes,
    /// Changes the audio sent into it (an effect).
    Changes,
    /// Both: plays from MIDI and also processes incoming audio
    /// (vocoders, samplers/synths with an audio input, MIDI-driven effects).
    Both,
    /// Neither test produced sound (e.g. a meter or analyzer).
    Neither,
}

impl SoundRole {
    pub fn as_kind(self) -> &'static str {
        match self {
            SoundRole::Makes => "Instrument",
            SoundRole::Changes | SoundRole::Neither => "Effect",
            SoundRole::Both => "Both",
        }
    }
}

fn rms_of(buf: &[f32]) -> f64 {
    if buf.is_empty() {
        return 0.0;
    }
    (buf.iter().map(|s| (*s as f64).powi(2)).sum::<f64>() / buf.len() as f64).sqrt()
}

/// Run `blocks` blocks: optional tone input and MIDI notes; returns output RMS.
fn run_test(p: &mut Plugin, blocks: usize, tone: bool, notes: bool) -> f64 {
    let mut out = Vec::with_capacity(blocks * BLOCK * 2);
    for b in 0..blocks {
        let mut l = vec![0f32; BLOCK];
        let mut r = vec![0f32; BLOCK];
        if tone {
            for i in 0..BLOCK {
                let t = (b * BLOCK + i) as f32 / SAMPLE_RATE as f32;
                l[i] = 0.25 * (2.0 * std::f32::consts::PI * 440.0 * t).sin();
                r[i] = l[i];
            }
        }
        // A C-major chord on the first block (covers split/zoned instruments).
        let midi: &[[u8; 3]] = if notes && b == 0 { &[[0x90, 48, 110], [0x90, 60, 110], [0x90, 64, 110], [0x90, 67, 110]] } else { &[] };
        p.process(&mut l, &mut r, midi);
        // Skip the first few blocks so start-up clicks don't count.
        if b >= 4 {
            out.extend_from_slice(&l);
            out.extend_from_slice(&r);
        }
    }
    rms_of(&out)
}

/// Decide whether a plugin makes sound, changes sound, or both, by running
/// it on test signals (each test on a fresh instance so tails can't leak
/// from one test into the next). Takes a few seconds; run it in a child
/// process (the plugin scan does).
pub fn sound_check(desc: &PluginDesc) -> Result<SoundRole, String> {
    let blocks = (SAMPLE_RATE as usize * 6 / 10) / BLOCK; // ~0.6 s per test
    let as_probe = PluginDesc { kind: "Both".into(), ..desc.clone() }; // never zero the input during tests

    let mut p = Plugin::load(&as_probe, None)?;
    let takes_audio = p.in_buses.first().is_some_and(|b| !b.channels.is_empty());
    let takes_midi = p.has_events;
    let base = run_test(&mut p, blocks, false, false);
    drop(p);

    let makes = if takes_midi {
        let mut p = Plugin::load(&as_probe, None)?;
        let e = run_test(&mut p, blocks, false, true);
        e > (base * 4.0).max(1e-4)
    } else {
        false
    };
    let changes = if takes_audio {
        let mut p = Plugin::load(&as_probe, None)?;
        let e = run_test(&mut p, blocks, true, false);
        e > (base * 4.0).max(1e-4)
    } else {
        false
    };
    Ok(match (makes, changes) {
        (true, true) => SoundRole::Both,
        (true, false) => SoundRole::Makes,
        (false, true) => SoundRole::Changes,
        (false, false) => SoundRole::Neither,
    })
}

impl PluginDesc {
    /// Makes sound from MIDI (instrument or both).
    pub fn makes_sound(&self) -> bool {
        self.kind == "Instrument" || self.kind == "Both"
    }
    /// Takes audio in (effect or both).
    pub fn takes_audio(&self) -> bool {
        self.kind == "Effect" || self.kind == "Both"
    }
}

impl Plugin {

    /// Process one block in place (`l`, `r` are BLOCK frames or fewer).
    /// `midi` are raw 3-byte MIDI messages for this block.
    pub fn process(&mut self, l: &mut [f32], r: &mut [f32], midi: &[[u8; 3]]) {
        let n = l.len().min(r.len()).min(BLOCK);
        if self.bypass {
            if self.desc.is_instrument() {
                l[..n].fill(0.0);
                r[..n].fill(0.0);
            }
            return;
        }
        unsafe {
            // Parameter edits from the editor, then MIDI.
            self.params_in.clear();
            self.params_out.clear();
            for (id, v) in self.pending.lock().unwrap().drain(..) {
                self.params_in.add(id, v);
            }
            {
                let mut evs = self.events.events.lock().unwrap();
                evs.clear();
                for m in midi {
                    let (status, ch) = (m[0] & 0xF0, (m[0] & 0x0F) as i16);
                    let mut e: Event = std::mem::zeroed();
                    e.flags = Event_::EventFlags_::kIsLive as u16;
                    match status {
                        0x90 if m[2] > 0 => {
                            e.r#type = Event_::EventTypes_::kNoteOnEvent as u16;
                            e.__field0.noteOn = NoteOnEvent { channel: ch, pitch: m[1] as i16, tuning: 0.0, velocity: m[2] as f32 / 127.0, length: 0, noteId: -1 };
                            evs.push(e);
                        }
                        0x80 | 0x90 => {
                            e.r#type = Event_::EventTypes_::kNoteOffEvent as u16;
                            e.__field0.noteOff = NoteOffEvent { channel: ch, pitch: m[1] as i16, velocity: m[2] as f32 / 127.0, noteId: -1, tuning: 0.0 };
                            evs.push(e);
                        }
                        0xB0 => {
                            if let Some(&id) = self.midi_map.get(&(m[1] as u16)) {
                                self.params_in.add(id, m[2] as f64 / 127.0);
                            }
                        }
                        0xD0 => {
                            if let Some(&id) = self.midi_map.get(&128) {
                                self.params_in.add(id, m[1] as f64 / 127.0);
                            }
                        }
                        0xE0 => {
                            if let Some(&id) = self.midi_map.get(&129) {
                                let v = (m[1] as u32 | ((m[2] as u32) << 7)) as f64 / 16383.0;
                                self.params_in.add(id, v);
                            }
                        }
                        _ => {}
                    }
                }
            }

            // Audio in.
            if let Some(bus) = self.in_buses.first_mut() {
                match bus.channels.len() {
                    0 => {}
                    1 => {
                        for i in 0..n {
                            bus.channels[0][i] = 0.5 * (l[i] + r[i]);
                        }
                    }
                    _ => {
                        bus.channels[0][..n].copy_from_slice(&l[..n]);
                        bus.channels[1][..n].copy_from_slice(&r[..n]);
                    }
                }
            }
            for bus in &mut self.out_buses {
                for c in &mut bus.channels {
                    c[..n].fill(0.0);
                }
            }
            let mut ins: Vec<AudioBusBuffers> = self.in_buses.iter_mut().map(|b| b.buffers()).collect();
            let mut outs: Vec<AudioBusBuffers> = self.out_buses.iter_mut().map(|b| b.buffers()).collect();
            let mut data = ProcessData {
                processMode: ProcessModes_::kRealtime as int32,
                symbolicSampleSize: SymbolicSampleSizes_::kSample32 as int32,
                numSamples: n as int32,
                numInputs: ins.len() as int32,
                numOutputs: outs.len() as int32,
                inputs: if ins.is_empty() { std::ptr::null_mut() } else { ins.as_mut_ptr() },
                outputs: if outs.is_empty() { std::ptr::null_mut() } else { outs.as_mut_ptr() },
                inputParameterChanges: self.params_in.as_com_ref::<IParameterChanges>().unwrap().as_ptr(),
                outputParameterChanges: self.params_out.as_com_ref::<IParameterChanges>().unwrap().as_ptr(),
                inputEvents: if self.has_events { self.events.as_com_ref::<IEventList>().unwrap().as_ptr() } else { std::ptr::null_mut() },
                outputEvents: std::ptr::null_mut(),
                processContext: &mut self.context,
            };
            self.processor.process(&mut data);
            self.context.projectTimeSamples += n as i64;
            self.context.continousTimeSamples += n as i64;

            // Audio out.
            if let Some(bus) = self.out_buses.first() {
                match bus.channels.len() {
                    0 => {}
                    1 => {
                        l[..n].copy_from_slice(&bus.channels[0][..n]);
                        r[..n].copy_from_slice(&bus.channels[0][..n]);
                    }
                    _ => {
                        l[..n].copy_from_slice(&bus.channels[0][..n]);
                        r[..n].copy_from_slice(&bus.channels[1][..n]);
                    }
                }
            }
        }
    }

    /// Save the plugin's state. Call on the UI thread.
    pub fn state(&self) -> PluginState {
        unsafe {
            let comp = ComWrapper::new(MemStream::default());
            self.component.getState(comp.as_com_ref::<IBStream>().unwrap().as_ptr());
            let ctrl = ComWrapper::new(MemStream::default());
            if let Some(c) = &self.controller {
                c.getState(ctrl.as_com_ref::<IBStream>().unwrap().as_ptr());
            }
            let a = comp.inner.lock().unwrap().0.clone();
            let b = ctrl.inner.lock().unwrap().0.clone();
            (a, b)
        }
    }

    /// Open the plugin's own editor in a pop-up window. Call on the UI thread.
    pub fn open_editor(&self, title: &str) -> Result<Editor, String> {
        let controller = self.controller.clone().ok_or("this plugin has no editor")?;
        unsafe { Editor::open(controller, title) }
    }
}

impl Drop for Plugin {
    fn drop(&mut self) {
        unsafe {
            self.processor.setProcessing(0);
            self.component.setActive(0);
            if let Some((a, b)) = self.links.take() {
                a.disconnect(b.as_ptr());
                b.disconnect(a.as_ptr());
            }
            if let Some(c) = &self.controller {
                c.setComponentHandler(std::ptr::null_mut());
                if self.separate_controller {
                    c.terminate();
                }
            }
            self.component.terminate();
        }
    }
}

// ===========================================================================
// Editor pop-up window
// ===========================================================================

use windows::core::w;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::*;

fn closed_windows() -> &'static Mutex<HashSet<isize>> {
    static CLOSED: OnceLock<Mutex<HashSet<isize>>> = OnceLock::new();
    CLOSED.get_or_init(Default::default)
}

unsafe extern "system" fn editor_wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if msg == WM_CLOSE {
        // Don't destroy yet: the host must detach the plugin view first.
        closed_windows().lock().unwrap().insert(hwnd.0 as isize);
        let _ = ShowWindow(hwnd, SW_HIDE);
        return LRESULT(0);
    }
    DefWindowProcW(hwnd, msg, wp, lp)
}

const EDITOR_STYLE: WINDOW_STYLE = WINDOW_STYLE(WS_OVERLAPPED.0 | WS_CAPTION.0 | WS_SYSMENU.0 | WS_MINIMIZEBOX.0 | WS_CLIPCHILDREN.0);

fn outer_size(w: i32, h: i32) -> (i32, i32) {
    let mut r = RECT { left: 0, top: 0, right: w, bottom: h };
    unsafe {
        let _ = AdjustWindowRectEx(&mut r, EDITOR_STYLE, false, WINDOW_EX_STYLE(0));
    }
    (r.right - r.left, r.bottom - r.top)
}

struct Frame {
    hwnd: Mutex<isize>,
}
impl Class for Frame {
    type Interfaces = (IPlugFrame,);
}
impl IPlugFrameTrait for Frame {
    unsafe fn resizeView(&self, view: *mut IPlugView, new_size: *mut ViewRect) -> tresult {
        let s = &*new_size;
        let (w, h) = outer_size(s.right - s.left, s.bottom - s.top);
        let hwnd = HWND(*self.hwnd.lock().unwrap() as *mut c_void);
        let _ = SetWindowPos(hwnd, None, 0, 0, w, h, SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE);
        if let Some(v) = ComRef::from_raw(view) {
            v.onSize(new_size);
        }
        kResultOk
    }
}

pub struct Editor {
    view: ComPtr<IPlugView>,
    _controller: ComPtr<IEditController>,
    frame: ComWrapper<Frame>,
    hwnd: HWND,
}

impl Editor {
    unsafe fn open(controller: ComPtr<IEditController>, title: &str) -> Result<Editor, String> {
        use windows::core::HSTRING;
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;
        let view = ComPtr::from_raw(controller.createView(ViewType::kEditor)).ok_or("plugin has no editor view")?;
        if view.isPlatformTypeSupported(kPlatformTypeHWND) != kResultTrue {
            return Err("plugin editor doesn't support Windows".into());
        }
        let mut rect: ViewRect = std::mem::zeroed();
        view.getSize(&mut rect);
        let (w, h) = outer_size((rect.right - rect.left).max(200), (rect.bottom - rect.top).max(100));

        let instance = GetModuleHandleW(None).map_err(|e| e.message())?;
        static REGISTER: OnceLock<()> = OnceLock::new();
        REGISTER.get_or_init(|| {
            let wc = WNDCLASSW {
                lpfnWndProc: Some(editor_wndproc),
                hInstance: instance.into(),
                lpszClassName: w!("MagicMixerPluginWindow"),
                hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
                ..Default::default()
            };
            RegisterClassW(&wc);
        });
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("MagicMixerPluginWindow"),
            &HSTRING::from(title),
            EDITOR_STYLE,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            w,
            h,
            None,
            None,
            instance,
            None,
        )
        .map_err(|e| format!("couldn't create window: {}", e.message()))?;

        let frame = ComWrapper::new(Frame { hwnd: Mutex::new(hwnd.0 as isize) });
        view.setFrame(frame.as_com_ref::<IPlugFrame>().unwrap().as_ptr());
        if view.attached(hwnd.0, kPlatformTypeHWND) != kResultOk {
            let _ = DestroyWindow(hwnd);
            return Err("plugin editor failed to open".into());
        }
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
        Ok(Editor { view, _controller: controller, frame, hwnd })
    }

    /// True once the user has closed the window (then drop the Editor).
    pub fn is_closed(&self) -> bool {
        closed_windows().lock().unwrap().contains(&(self.hwnd.0 as isize))
    }

    pub fn focus(&self) {
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_RESTORE);
            let _ = SetForegroundWindow(self.hwnd);
        }
    }
}

impl Drop for Editor {
    fn drop(&mut self) {
        unsafe {
            self.view.removed();
            self.view.setFrame(std::ptr::null_mut());
            let _ = DestroyWindow(self.hwnd);
            closed_windows().lock().unwrap().remove(&(self.hwnd.0 as isize));
        }
        let _ = &self.frame;
    }
}
