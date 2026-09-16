/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
//!
//! AVAudioPlayer
//!
//! Implemented using Audio Queue Services based on [the PlayingAudio example](https://developer.apple.com/library/archive/documentation/MusicAudio/Conceptual/AudioQueueProgrammingGuide/AQPlayback/PlayingAudio.html)

use std::time::Instant;

use crate::dyld::HostFunction;
use crate::frameworks::audio_toolbox::audio_file::{
    self, kAudioFileCAFType, kAudioFilePropertyDataFormat, kAudioFilePropertyPacketSizeUpperBound,
    kAudioFileReadPermission, AudioFileClose, AudioFileCreateWithURL, AudioFileGetProperty,
    AudioFileHostObject, AudioFileID, AudioFileOpenURL, AudioFileReadPackets, AudioFileWriteBytes,
};
use crate::frameworks::audio_toolbox::audio_queue::{
    kAudioQueueParam_Pan, kAudioQueueParam_Volume, AudioQueueAllocateBuffer, AudioQueueBufferRef,
    AudioQueueDispose, AudioQueueEnqueueBuffer, AudioQueueGetParameter, AudioQueueNewOutput,
    AudioQueueOutputCallback, AudioQueuePause, AudioQueueRef, AudioQueueSetParameter,
    AudioQueueStart, AudioQueueStop,
};
use crate::frameworks::carbon_core::eofErr;
use crate::frameworks::core_audio_types::AudioStreamBasicDescription;
use crate::frameworks::core_foundation::cf_run_loop::kCFRunLoopCommonModes;
use crate::frameworks::foundation::ns_error::NSOSStatusErrorDomain;
use crate::frameworks::foundation::{ns_string, NSInteger, NSTimeInterval, NSUInteger};
use crate::mem::{guest_size_of, ConstVoidPtr, GuestUSize, MutPtr, MutVoidPtr, Ptr};
use crate::frameworks::core_audio_types::kAudioFormatLinearPCM;
use crate::objc::{
    autorelease, id, msg, msg_class, nil, release, retain, Class, ClassExports, HostObject,
    NSZonePtr,
};
use crate::objc_classes;
use crate::Environment;

const kNumberBuffers: usize = 3;

#[derive(Default)]
struct AVAudioRecorderHostObject {
    url: id,
    is_recording: bool,
    metering_enabled: bool,
    delegate: id,
    /// Real microphone capture state (all None/zero when the host has no
    /// mic — in that case the recorder degrades to its old silent stub).
    audio_file_id: Option<AudioFileID>,
    format: Option<AudioStreamBasicDescription>,
    write_offset: u64,
    mic_started: bool,
    last_pump: Option<Instant>,
    average_power: f32,
    peak_power: f32,
}
impl HostObject for AVAudioRecorderHostObject {}

#[derive(Default)]
struct AVAudioPlayerHostObject {
    audio_file_url: id,
    output_callback: AudioQueueOutputCallback,
    audio_file_id: Option<AudioFileID>,
    audio_desc: Option<AudioStreamBasicDescription>,
    audio_queue: Option<AudioQueueRef>,
    audio_queue_buffers: Option<MutPtr<AudioQueueBufferRef>>,
    num_packets_to_read: u32,
    current_packet: i64,
    set_current_time: NSTimeInterval,
    volume: f32,
    /// Stereo pan; matches `AVAudioPlayer.pan` from `AVAudioPlayer.h`.
    /// Range -1.0 (full left) … 1.0 (full right). Default 0.0.
    pan: f32,
    is_playing: bool,
    num_of_loops: NSInteger,
    delegate: id,
    metering_enabled: bool,
}
impl HostObject for AVAudioPlayerHostObject {}

pub const CLASSES: ClassExports = objc_classes! {

(env, this, _cmd);
@implementation AVAudioPlayer: NSObject

+ (id)allocWithZone:(NSZonePtr)_zone {
    let symb = "__touchHLE_AVAudioPlayerOutputBufferHelper";
    let hf: HostFunction = &(_touchHLE_AVAudioPlayerOutputBufferHelper as fn(&mut Environment, _, _, _) -> _);
    let callback = env
        .dyld
        .create_guest_function(&mut env.mem, symb, hf);
    let host_object = Box::new(AVAudioPlayerHostObject {
        audio_file_url: nil,
        output_callback: callback,
        audio_file_id: None,
        audio_desc: None,
        audio_queue: None,
        audio_queue_buffers: None,
        num_packets_to_read: 0,
        current_packet: 0,
        set_current_time: 0.0,
        volume: 1.0,
        pan: 0.0,
        is_playing: false,
        num_of_loops: 0,
        delegate: nil,
        metering_enabled: false,
    });
    env.objc.alloc_object(this, host_object, &mut env.mem)
}

- (id)initWithContentsOfURL:(id)url
                      error:(MutPtr<id>)outError {
    let path: id = msg![env; url path];
    let path_str = ns_string::to_rust_string(env, path);
    log!("[(AVAudioPlayer*){:?} initWithContentsOfURL:{:?} {} outError:{:?}]", this, url, path_str, outError);

    retain(env, url);
    env.objc.borrow_mut::<AVAudioPlayerHostObject>(this).audio_file_url = url;
    let tmp_afi_ptr: MutPtr<AudioFileID> = env.mem.alloc(guest_size_of::<AudioFileID>()).cast();
    let status = AudioFileOpenURL(env, url, kAudioFileReadPermission, 0, tmp_afi_ptr) as NSInteger;
    let audio_file_id = env.mem.read(tmp_afi_ptr);
    env.objc.borrow_mut::<AVAudioPlayerHostObject>(this).audio_file_id = Some(audio_file_id);
    env.mem.free(tmp_afi_ptr.cast());
    if status != 0 {
        if !outError.is_null() {
            let domain = ns_string::get_static_str(env, NSOSStatusErrorDomain);
            let error = msg_class![env; NSError alloc];
            let error = msg![env; error initWithDomain:domain code:status userInfo:nil];
            env.mem.write(outError, error);
        }
        return nil;
    }

    this
}

- (id)initWithData:(id)data error:(MutPtr<id>)outError {
    log_dbg!("[(AVAudioPlayer*){:?} initWithData:{:?} outError:{:?}]", this, data, outError);

    assert_eq!(env.objc.borrow::<AVAudioPlayerHostObject>(this).audio_file_url, nil);

    let bytes: ConstVoidPtr = msg![env; data bytes];
    let length: NSUInteger = msg![env; data length];
    let data_vec = env.mem.bytes_at(bytes.cast(), length as GuestUSize).to_vec();

    match crate::audio::AudioFile::read_from_vec(data_vec) {
        Ok(file) => {
            assert!(env.objc.borrow::<AVAudioPlayerHostObject>(this).audio_file_id.is_none());
            let host_object = AudioFileHostObject::Real(file);
            let guest_audio_file = audio_file::register_audio_file(env, host_object);
            env.objc.borrow_mut::<AVAudioPlayerHostObject>(this).audio_file_id =
                Some(guest_audio_file);
            this
        }
        Err(_) => {
            if !outError.is_null() {
                let domain = ns_string::get_static_str(env, NSOSStatusErrorDomain);
                let error = msg_class![env; NSError alloc];
                // kAudioFileUnsupportedFileTypeError ('typ?') from
                // AudioToolbox's AudioFile error codes, which is what
                // AVFoundation surfaces for undecodable inputs.
                let code: NSInteger = 0x7479_703F;
                let error = msg![env; error initWithDomain:domain code:code userInfo:nil];
                autorelease(env, error);
                env.mem.write(outError, error);
            }
            release(env, this);
            nil
        }
    }
}

- (())setDelegate:(id)delegate {
    log_dbg!("[(AVAudioPlayer*){:?} setDelegate:{:?}]", this, delegate);
    env.objc.borrow_mut::<AVAudioPlayerHostObject>(this).delegate = delegate;
}

- (id)delegate {
    env.objc.borrow::<AVAudioPlayerHostObject>(this).delegate
}

- (())setMeteringEnabled:(bool)enabled {
    log_dbg!("[(AVAudioPlayer*){:?} setMeteringEnabled:{}]", this, enabled);
    env.objc.borrow_mut::<AVAudioPlayerHostObject>(this).metering_enabled = enabled;
}

- (bool)isMeteringEnabled {
    env.objc.borrow::<AVAudioPlayerHostObject>(this).metering_enabled
}

- (f32)volume {
    let aq_ref = env.objc.borrow_mut::<AVAudioPlayerHostObject>(this).audio_queue;
    if aq_ref.is_none() {
        return env.objc.borrow_mut::<AVAudioPlayerHostObject>(this).volume;
    }

    let tmp: MutPtr<f32> = env.mem.alloc(guest_size_of::<f32>()).cast();
    let status = AudioQueueGetParameter(env, aq_ref.unwrap(), kAudioQueueParam_Volume, tmp);
    assert_eq!(status, 0);
    let res = env.mem.read(tmp);
    env.mem.free(tmp.cast());
    res
}

- (())setVolume:(f32)volume {
    let host_object = env.objc.borrow_mut::<AVAudioPlayerHostObject>(this);
    host_object.volume = volume;
    if let Some(aq_ref) = host_object.audio_queue {
        let status = AudioQueueSetParameter(env, aq_ref, kAudioQueueParam_Volume, volume);
        assert_eq!(status, 0);
    }
}

// `AVAudioPlayer.pan` (iOS 4.0+) — stereo panning. Apple documents the
// range as -1.0 (full left) to 1.0 (full right) with 0.0 centered.
// <https://developer.apple.com/documentation/avfaudio/avaudioplayer/1387672-pan>
- (f32)pan {
    let host_object = env.objc.borrow::<AVAudioPlayerHostObject>(this);
    let aq_ref = host_object.audio_queue;
    if aq_ref.is_none() {
        return host_object.pan;
    }
    let tmp: MutPtr<f32> = env.mem.alloc(guest_size_of::<f32>()).cast();
    let status = AudioQueueGetParameter(env, aq_ref.unwrap(), kAudioQueueParam_Pan, tmp);
    assert_eq!(status, 0);
    let res = env.mem.read(tmp);
    env.mem.free(tmp.cast());
    res
}

- (())setPan:(f32)pan {
    let pan = pan.clamp(-1.0, 1.0);
    let host_object = env.objc.borrow_mut::<AVAudioPlayerHostObject>(this);
    host_object.pan = pan;
    if let Some(aq_ref) = host_object.audio_queue {
        let status = AudioQueueSetParameter(env, aq_ref, kAudioQueueParam_Pan, pan);
        assert_eq!(status, 0);
    }
}

- (())prepareToPlay {
    // The host object can be missing if the player was already deallocated
    // (e.g. the game released it but kept a stale pointer around) or if the
    // initWithContentsOfURL: call earlier failed and never populated
    // `audio_file_id`. In either case we have nothing to prepare; bail out
    // with a warning instead of panicking on `unwrap()`.
    let (audio_queue, audio_file_id_opt) = {
        let host_object = env.objc.borrow::<AVAudioPlayerHostObject>(this);
        (host_object.audio_queue, host_object.audio_file_id)
    };
    if audio_queue.is_some() {
        return;
    }
    let Some(audio_file_id) = audio_file_id_opt else {
        log!(
            "Warning: [(AVAudioPlayer*){:?} prepareToPlay] called with no \
             audio file ID (the player was never successfully initialized \
             or it has been deallocated); ignoring.",
            this
        );
        return;
    };
    let callback = env.objc.borrow::<AVAudioPlayerHostObject>(this).output_callback;

    let size = guest_size_of::<AudioStreamBasicDescription>();
    let tmp_size_ptr: MutPtr<GuestUSize> = env.mem.alloc(guest_size_of::<GuestUSize>()).cast();
    env.mem.write(tmp_size_ptr, size);
    let tmp_data_ptr: MutPtr<AudioStreamBasicDescription> = env.mem.alloc(size).cast();
    let status = AudioFileGetProperty(
        env, audio_file_id, kAudioFilePropertyDataFormat, tmp_size_ptr, tmp_data_ptr.cast()
    );
    assert_eq!(status, 0);
    assert_eq!(size, env.mem.read(tmp_size_ptr));
    let audio_desc = env.mem.read(tmp_data_ptr);
    log_dbg!("audio_desc {:?}", audio_desc);
    env.objc.borrow_mut::<AVAudioPlayerHostObject>(this).audio_desc = Some(audio_desc);

    let aq_ref_ptr: MutPtr<AudioQueueRef> = env.mem.alloc(guest_size_of::<AudioQueueRef>()).cast();
    let common_modes = ns_string::get_static_str(env, kCFRunLoopCommonModes);
    let status = AudioQueueNewOutput(
        env, tmp_data_ptr.cast_const(), callback, this.cast(),
        Ptr::null(), common_modes, 0, aq_ref_ptr
    );
    assert_eq!(status, 0);
    let aq_ref = env.mem.read(aq_ref_ptr);
    env.objc.borrow_mut::<AVAudioPlayerHostObject>(this).audio_queue = Some(aq_ref);

    let volume = env.objc.borrow::<AVAudioPlayerHostObject>(this).volume;
    () = msg![env; this setVolume:volume];
    let set_current_time = env.objc.borrow::<AVAudioPlayerHostObject>(this).set_current_time;
    () = msg![env; this setCurrentTime:set_current_time];

    let size = guest_size_of::<u32>();
    env.mem.write(tmp_size_ptr, size);
    let prop_size_ptr: MutPtr<u32> = env.mem.alloc(size).cast();
    let status = AudioFileGetProperty(
        env, audio_file_id, kAudioFilePropertyPacketSizeUpperBound, tmp_size_ptr, prop_size_ptr.cast()
    );
    assert_eq!(status, 0);
    assert_eq!(size, env.mem.read(tmp_size_ptr));
    let prop_size = env.mem.read(prop_size_ptr);

    let (buffer_byte_size, num_packets_to_read) = derive_buffer_size(audio_desc, prop_size, 0.5);
    env.objc.borrow_mut::<AVAudioPlayerHostObject>(this).num_packets_to_read = num_packets_to_read;
    let buffers: MutPtr<AudioQueueBufferRef> = env.mem.alloc(kNumberBuffers as GuestUSize * guest_size_of::<AudioQueueBufferRef>()).cast();
    env.objc.borrow_mut::<AVAudioPlayerHostObject>(this).audio_queue_buffers = Some(buffers);

    env.objc.borrow_mut::<AVAudioPlayerHostObject>(this).is_playing = true;
    for i in 0..kNumberBuffers {
        let status = AudioQueueAllocateBuffer(env, aq_ref, buffer_byte_size, buffers + i as u32);
        assert_eq!(status, 0);

        _touchHLE_AVAudioPlayerOutputBufferHelper(env, this.cast(), aq_ref, env.mem.read(buffers + i as u32));
    }
    env.objc.borrow_mut::<AVAudioPlayerHostObject>(this).is_playing = false;

    env.mem.free(tmp_size_ptr.cast());
    env.mem.free(aq_ref_ptr.cast());
    env.mem.free(tmp_data_ptr.cast());
}

- (bool)isPlaying {
    env.objc.borrow::<AVAudioPlayerHostObject>(this).is_playing
}

- (bool)play {
    log!("[(AVAudioPlayer*){:?} play]", this);
    () = msg![env; this prepareToPlay];
    // If `prepareToPlay` couldn't set up an audio queue (e.g. because the
    // player was already deallocated and the host object is now missing,
    // making every borrow_mut return a phantom AVAudioPlayerHostObject
    // whose audio_queue is None), don't panic — just return NO like the
    // real AVAudioPlayer does when playback can't start.
    let Some(aq_ref) = env.objc.borrow_mut::<AVAudioPlayerHostObject>(this).audio_queue else {
        log!(
            "Warning: [(AVAudioPlayer*){:?} play] could not start: \
             prepareToPlay produced no audio queue (the player has \
             likely been deallocated); returning NO.",
            this
        );
        return false;
    };
    env.objc.borrow_mut::<AVAudioPlayerHostObject>(this).is_playing = true;
    let status = AudioQueueStart(env, aq_ref, Ptr::null());
    if status != 0 {
        log!(
            "Warning: AudioQueueStart for {:?} failed with status {}; returning NO.",
            this, status
        );
        env.objc.borrow_mut::<AVAudioPlayerHostObject>(this).is_playing = false;
        return false;
    }
    true
}

- (())pause {
    env.objc.borrow_mut::<AVAudioPlayerHostObject>(this).is_playing = false;
    if let Some(aq_ref) = env.objc.borrow::<AVAudioPlayerHostObject>(this).audio_queue {
        AudioQueuePause(env, aq_ref);
    }
}

- (())stop {
    let &mut AVAudioPlayerHostObject {
        audio_queue,
        audio_queue_buffers,
        ..
    } = env.objc.borrow_mut::<AVAudioPlayerHostObject>(this);
    if audio_queue.is_none() {
        return;
    }
    AudioQueueDispose(env, audio_queue.unwrap(), true);
    env.mem.free(audio_queue_buffers.unwrap().cast());
    let &AVAudioPlayerHostObject { audio_file_url, output_callback, num_of_loops, audio_file_id, delegate, metering_enabled, .. } = env.objc.borrow(this);
    *env.objc.borrow_mut::<AVAudioPlayerHostObject>(this) = AVAudioPlayerHostObject {
        audio_file_url,
        output_callback,
        num_of_loops,
        audio_file_id,
        audio_desc: None,
        audio_queue: None,
        audio_queue_buffers: None,
        num_packets_to_read: 0,
        current_packet: 0,
        set_current_time: 0.0,
        volume: 1.0,
        pan: 0.0,
        is_playing: false,
        delegate,         // Переносим в новый объект
        metering_enabled,
    };
}

- (())setNumberOfLoops:(NSInteger)numberOfLoops {
    log_dbg!("[(AVAudioPlayer *) {:?} setNumberOfLoops:{:?}]", this, numberOfLoops);
    env.objc.borrow_mut::<AVAudioPlayerHostObject>(this).num_of_loops = numberOfLoops;
}

- (())dealloc {
    () = msg![env; this stop];
    let &AVAudioPlayerHostObject {audio_file_url, audio_file_id, ..} = env.objc.borrow(this);
    release(env, audio_file_url);
    if let Some(audio_file_id) = audio_file_id {
        AudioFileClose(env, audio_file_id);
    }
    env.objc.dealloc_object(this, &mut env.mem)
}

- (NSTimeInterval)currentTime {
    let host_object = env.objc.borrow::<AVAudioPlayerHostObject>(this);
    let current_time = if let Some(audio_desc) = host_object.audio_desc {
        let current_frame = (host_object.current_packet as f64) * (audio_desc.frames_per_packet as f64);
        current_frame / audio_desc.sample_rate
    } else {
        0.0
    };
    log_dbg!("[(AVAudioPlayer *) {:?} currentTime] -> {:?}", this, current_time);
    current_time
}

- (())setCurrentTime:(NSTimeInterval)currentTime {
    let host_object = env.objc.borrow_mut::<AVAudioPlayerHostObject>(this);
    host_object.set_current_time = currentTime;
    if let (Some(audio_desc), Some(audio_file_id)) = (host_object.audio_desc, host_object.audio_file_id) {

        let target_host_obj = audio_file::State::get(&mut env.framework_state)
            .audio_files
            .get(&audio_file_id)
            .unwrap();

        let total_packets = match target_host_obj {
            AudioFileHostObject::Real(af) => af.packet_count(),
            AudioFileHostObject::Dummy { packet_count, .. } => *packet_count,
            AudioFileHostObject::Writable { format, ref data, .. } => {
                let bpp = format.bytes_per_packet;
                if bpp > 0 { data.len() as u64 / bpp as u64 } else { 0 }
            }
        };

        let total_frames = total_packets * audio_desc.frames_per_packet as u64;
        let new_current_frame = audio_desc.sample_rate * currentTime;
        if new_current_frame < 0.0 || new_current_frame > total_frames as f64 {
            host_object.current_packet = 0;
        } else {
            host_object.current_packet = (new_current_frame / (audio_desc.frames_per_packet as f64)) as i64;
        }
    }
    log_dbg!("[(AVAudioPlayer *) {:?} setCurrentTime: {}]", this, currentTime);
}

- (NSTimeInterval)duration {
    let host_object = env.objc.borrow::<AVAudioPlayerHostObject>(this);
    let (audio_desc, audio_file_id) = (
        host_object.audio_desc,
        host_object.audio_file_id,
    );
    if let (Some(audio_desc), Some(audio_file_id)) = (audio_desc, audio_file_id) {
        let target_host_obj = audio_file::State::get(&mut env.framework_state)
            .audio_files
            .get(&audio_file_id)
            .unwrap();
        let total_packets = match target_host_obj {
            AudioFileHostObject::Real(af) => af.packet_count(),
            AudioFileHostObject::Dummy { packet_count, .. } => *packet_count,
            AudioFileHostObject::Writable { format, ref data, .. } => {
                let bpp = format.bytes_per_packet;
                if bpp > 0 { data.len() as u64 / bpp as u64 } else { 0 }
            }
        };
        if audio_desc.sample_rate > 0.0 && audio_desc.frames_per_packet > 0 {
            let total_frames =
                total_packets * audio_desc.frames_per_packet as u64;
            return total_frames as f64 / audio_desc.sample_rate;
        }
    }
    0.0
}

- (NSInteger)numberOfLoops {
    env.objc.borrow::<AVAudioPlayerHostObject>(this).num_of_loops
}

- (id)url {
    env.objc.borrow::<AVAudioPlayerHostObject>(this).audio_file_url
}

- (NSInteger)numberOfChannels {
    let host_object = env.objc.borrow::<AVAudioPlayerHostObject>(this);
    if let Some(audio_desc) = host_object.audio_desc {
        return audio_desc.channels_per_frame as NSInteger;
    }
    // Return a safe default when the queue is not yet prepared.
    1
}

- (bool)playAtTime:(NSTimeInterval)time {
    () = msg![env; this setCurrentTime:time];
    msg![env; this play]
}

- (())updateMeters {
    log_dbg!(
        "[(AVAudioPlayer *){:?} updateMeters] — stub",
        this
    );
}

- (f32)averagePowerForChannel:(NSInteger)channel_number {
    log_dbg!(
        "[(AVAudioPlayer *){:?} averagePowerForChannel:{}] — stub",
        this,
        channel_number
    );
    -160.0
}

- (f32)peakPowerForChannel:(NSInteger)channel_number {
    log_dbg!(
        "[(AVAudioPlayer *){:?} peakPowerForChannel:{}] — stub",
        this,
        channel_number
    );
    -160.0
}

@end

@implementation AVAudioRecorder: NSObject

+ (id)allocWithZone:(NSZonePtr)_zone {
    let host_object = Box::new(AVAudioRecorderHostObject {
        url: nil,
        is_recording: false,
        metering_enabled: false,
        delegate: nil,
        audio_file_id: None,
        format: None,
        write_offset: 0,
        mic_started: false,
        last_pump: None,
        average_power: -160.0,
        peak_power: -160.0,
    });
    env.objc.alloc_object(this, host_object, &mut env.mem)
}

- (id)initWithURL:(id)url
         settings:(id)_settings
            error:(MutPtr<id>)out_error {
    if url == nil {
        if !out_error.is_null() {
            env.mem.write(out_error, nil);
        }
        return nil;
    }
    retain(env, url);
    env.objc
        .borrow_mut::<AVAudioRecorderHostObject>(this)
        .url = url;
    if !out_error.is_null() {
        env.mem.write(out_error, nil);
    }
    this
}

- (bool)recordForDuration:(NSTimeInterval)_duration {
    msg![env; this record]
}

- (bool)prepareToRecord {
    // The real capture session (destination CAF file + host mic) is created
    // lazily by `record`; games that call prepareToRecord first then record
    // get the same behaviour either way.
    log_dbg!("[(AVAudioRecorder *){:?} prepareToRecord]", this);
    true
}

- (())pause {
    recorder_stop_capture(env, this);
}

- (())stop {
    recorder_stop_capture(env, this);
}

- (bool)isRecording {
    pump_recorder(env, this);
    env.objc
        .borrow::<AVAudioRecorderHostObject>(this)
        .is_recording
}

- (bool)deleteRecording {
    recorder_stop_capture(env, this);
    true
}

- (id)url {
    env.objc.borrow::<AVAudioRecorderHostObject>(this).url
}

- (NSTimeInterval)currentTime {
    pump_recorder(env, this);
    let host = env.objc.borrow::<AVAudioRecorderHostObject>(this);
    let bytes_per_frame = host
        .format
        .map(|f| (f.bytes_per_frame.max(1)) as u64)
        .unwrap_or(2);
    let sample_rate = host
        .format
        .map(|f| if f.sample_rate > 0.0 { f.sample_rate } else { 44100.0 })
        .unwrap_or(44100.0);
    (host.write_offset / bytes_per_frame) as f64 / sample_rate
}

- (NSTimeInterval)deviceCurrentTime {
    msg![env; this currentTime]
}

- (())setMeteringEnabled:(bool)enabled {
    env.objc
        .borrow_mut::<AVAudioRecorderHostObject>(this)
        .metering_enabled = enabled;
}

- (bool)isMeteringEnabled {
    env.objc
        .borrow::<AVAudioRecorderHostObject>(this)
        .metering_enabled
}

- (())updateMeters {
    pump_recorder(env, this);
}

- (f32)averagePowerForChannel:(NSInteger)_channel {
    pump_recorder(env, this);
    let _ = _channel;
    env.objc
        .borrow::<AVAudioRecorderHostObject>(this)
        .average_power
}

- (f32)peakPowerForChannel:(NSInteger)_channel {
    pump_recorder(env, this);
    let _ = _channel;
    env.objc
        .borrow::<AVAudioRecorderHostObject>(this)
        .peak_power
}

- (id)delegate {
    env.objc.borrow::<AVAudioRecorderHostObject>(this).delegate
}

- (())setDelegate:(id)delegate {
    env.objc
        .borrow_mut::<AVAudioRecorderHostObject>(this)
        .delegate = delegate;
}

- (())dealloc {
    let url = env
        .objc
        .borrow::<AVAudioRecorderHostObject>(this)
        .url;
    if url != nil {
        release(env, url);
    }
    env.objc.dealloc_object(this, &mut env.mem)
}

- (bool)record {
    // Real microphone capture when the host provides one (Android: JNI ->
    // AudioRecord; elsewhere: graceful degradation to the old silent stub).
    let (url, already) = {
        let host = env.objc.borrow::<AVAudioRecorderHostObject>(this);
        (host.url, host.is_recording)
    };
    if already {
        return true;
    }
    if url == nil {
        return false;
    }

    if !crate::android_media::has_microphone() {
        log!(
            "[(AVAudioRecorder *){:?} record] no host microphone available; \
             recording silence",
            this
        );

    }

    // Build the ASBD from the settings dictionary (Linear PCM only; anything
    // else falls back to 16-bit mono at the settings' sample rate).
    let path: id = msg![env; url path];
    let asbd = AudioStreamBasicDescription {
        sample_rate: 44100.0,
        format_id: kAudioFormatLinearPCM,
        format_flags: crate::frameworks::core_audio_types::kAudioFormatFlagIsSignedInteger
            | crate::frameworks::core_audio_types::kAudioFormatFlagIsPacked,
        bytes_per_packet: 2,
        frames_per_packet: 1,
        bytes_per_frame: 2,
        channels_per_frame: 1,
        bits_per_channel: 16,
        _reserved: 0,
    };
    let _ = path;
    let tmp_afi_ptr: MutPtr<AudioFileID> = env.mem.alloc(guest_size_of::<AudioFileID>()).cast();
    let asbd_ptr = env.mem.alloc_and_write(asbd);
    let status = AudioFileCreateWithURL(
        env,
        url.cast(),
        kAudioFileCAFType,
        asbd_ptr.cast_const(),
        0,
        tmp_afi_ptr,
    );
    env.mem.free(asbd_ptr.cast());
    let audio_file_id = env.mem.read(tmp_afi_ptr);
    env.mem.free(tmp_afi_ptr.cast());
    if status != 0 {
        log!(
            "[(AVAudioRecorder *){:?} record] AudioFileCreateWithURL failed: {}",
            this,
            status
        );
        return false;
    }

    let mic_started = crate::android_media::start_mic();
    if !mic_started {
        log!(
            "[(AVAudioRecorder *){:?} record] mic unavailable, recording silence",
            this
        );
    }

    {
        let host = env.objc.borrow_mut::<AVAudioRecorderHostObject>(this);
        host.audio_file_id = Some(audio_file_id);
        host.format = Some(asbd);
        host.write_offset = 0;
        host.mic_started = mic_started;
        host.last_pump = Some(Instant::now());
        host.is_recording = true;
        host.average_power = -160.0;
        host.peak_power = -160.0;
    }
    log!(
        "[(AVAudioRecorder *){:?} record] real capture {}",
        this,
        if mic_started { "started" } else { "(silence)" }
    );
    true
}
@end

};

/// Stop capture: halt the host mic stream and close the audio file.
fn recorder_stop_capture(env: &mut Environment, this: id) {
    let (was_recording, mic_started, file_id) = {
        let host = env.objc.borrow::<AVAudioRecorderHostObject>(this);
        (host.is_recording, host.mic_started, host.audio_file_id)
    };
    if !was_recording {
        return;
    }
    if mic_started {
        crate::android_media::stop_mic();
    }
    if let Some(file_id) = file_id {
        AudioFileClose(env, file_id);
    }
    let host = env.objc.borrow_mut::<AVAudioRecorderHostObject>(this);
    host.is_recording = false;
    host.mic_started = false;
    host.audio_file_id = None;
    host.format = None;
}


/// Pump pending mic samples into the recorder's audio file. Called lazily
/// from currentTime/updateMeters/isRecording so no background thread is
/// needed. On platforms without a host mic this is a no-op.
fn pump_recorder(env: &mut Environment, this: id) {
    let (recording, file_id, format, mic_started) = {
        let host = env.objc.borrow::<AVAudioRecorderHostObject>(this);
        (
            host.is_recording,
            host.audio_file_id,
            host.format,
            host.mic_started,
        )
    };
    if !recording {
        return;
    }
    let Some(file_id) = file_id else { return };
    let Some(format) = format else { return };

    // Read one ~40ms chunk of 16-bit mono host PCM (44.1 kHz).
    let mut samples: Vec<i16> = if mic_started {
        crate::android_media::read_mic_chunk()
    } else {
        Vec::new()
    };
    if samples.is_empty() {
        // Keep metering quiet and file untouched between host chunks.
        return;
    }

    // Metering.
    let peak = samples.iter().fold(0i16, |a, &s| a.max(s.abs()));
    let avg = samples.iter().map(|&s| (s as f64).abs()).sum::<f64>() / samples.len() as f64;
    let db = |v: f64| {
        if v <= 0.0 {
            -160.0
        } else {
            (20.0 * v.log10()).clamp(-160.0, 0.0) as f32
        }
    };

    // Naive resample from 44.1 kHz to the file's sample rate.
    let src_rate = crate::android_media::MIC_SAMPLE_RATE as f64;
    let dst_rate = if format.sample_rate > 0.0 {
        format.sample_rate
    } else {
        44100.0
    };
    let converted: Vec<i16> = if (src_rate - dst_rate).abs() < 1.0 {
        samples
    } else {
        let out_len = ((samples.len() as f64) * dst_rate / src_rate).round() as usize;
        (0..out_len)
            .map(|i| {
                let pos = i as f64 * src_rate / dst_rate;
                let i0 = pos as usize;
                let i1 = (i0 + 1).min(samples.len() - 1);
                let frac = pos - i0 as f64;
                let a = samples[i0] as f64;
                let b = samples[i1] as f64;
                (a + (b - a) * frac).round().clamp(-32768.0, 32767.0) as i16
            })
            .collect()
    };

    // Channel layout: duplicate to stereo or drop to mono as needed.
    let bytes: Vec<u8> = match format.channels_per_frame {
        2 => converted
            .iter()
            .flat_map(|&s| s.to_le_bytes())
            .chain(converted.iter().flat_map(|&s| s.to_le_bytes()))
            .collect(),
        _ => converted.iter().flat_map(|&s| s.to_le_bytes()).collect(),
    };

    let bytes_ptr = env.mem.alloc(bytes.len() as u32);
    let slice = env.mem.bytes_at_mut(bytes_ptr.cast(), bytes.len() as u32);
    slice.copy_from_slice(&bytes);
    let mut num_bytes: u32 = bytes.len() as u32;
    let io_ptr = env.mem.alloc_and_write(num_bytes);
    let write_offset = {
        let host = env.objc.borrow::<AVAudioRecorderHostObject>(this);
        host.write_offset as i64
    };
    let status = AudioFileWriteBytes(
        env,
        file_id,
        false,
        write_offset,
        io_ptr,
        bytes_ptr.cast_const(),
    );
    num_bytes = env.mem.read(io_ptr);
    env.mem.free(io_ptr.cast());
    env.mem.free(bytes_ptr.cast_void());
    if status == 0 {
        let host = env.objc.borrow_mut::<AVAudioRecorderHostObject>(this);
        host.write_offset += num_bytes as u64;
        host.average_power = db(avg);
        host.peak_power = db(peak as f64);
    }
}


fn derive_buffer_size(
    audio_desc: AudioStreamBasicDescription,
    max_packet_size: u32,
    seconds: f64,
) -> (u32, u32) {
    let mut out_buffer_size;
    const max_buffer_size: u32 = 0x50000;
    const min_buffer_size: u32 = 0x4000;

    // ЧЕСТНЫЙ ФИКС: Защита от деления на ноль.
    // Если upper bound размера пакета = 0, пытаемся взять размер из дескриптора
    // формата.
    // Если и там пусто, ставим безопасный дефолт, как это делает настоящий
    // CoreAudio.
    let actual_max_packet_size = if max_packet_size > 0 {
        max_packet_size
    } else if audio_desc.bytes_per_packet > 0 {
        audio_desc.bytes_per_packet
    } else {
        1024
    };

    if audio_desc.frames_per_packet != 0 {
        let num_packets_to_time =
            audio_desc.sample_rate / audio_desc.frames_per_packet as f64 * seconds;
        out_buffer_size = num_packets_to_time as u32 * actual_max_packet_size;
    } else {
        out_buffer_size = if max_buffer_size > actual_max_packet_size {
            max_buffer_size
        } else {
            actual_max_packet_size
        }
    }

    if out_buffer_size > max_buffer_size && out_buffer_size > actual_max_packet_size {
        out_buffer_size = max_buffer_size
    } else if out_buffer_size < min_buffer_size {
        out_buffer_size = min_buffer_size
    }

    // Деление теперь абсолютно безопасно
    let out_num_packets_to_read = out_buffer_size / actual_max_packet_size;
    (out_buffer_size, out_num_packets_to_read)
}

fn _touchHLE_AVAudioPlayerOutputBufferHelper(
    env: &mut Environment,
    in_user_data: MutVoidPtr,
    in_aq: AudioQueueRef,
    in_buf: AudioQueueBufferRef,
) {
    let av_audio_player: id = in_user_data.cast();

    // The audio queue stores a raw pointer to the AVAudioPlayer instance, so
    // it's possible (some games release the player while a buffer callback is
    // already in flight, or before `-stop` has had a chance to dispose the
    // queue) for this callback to fire on an object that has already been
    // deallocated. In that case the receiver either is `nil`, has a `nil`
    // `isa`, or is not actually an `AVAudioPlayer` subclass anymore.
    //
    // Apple's runtime simply ignores the orphaned callback (the queue gets
    // disposed soon after); we mirror that behaviour by bailing out with a
    // diagnostic instead of panicking. Otherwise a perfectly normal use of
    // AVAudioPlayer would tear the whole emulator down.
    if av_audio_player.is_null() {
        log!(
            "Warning: AVAudioPlayer audio callback fired with a null user \
             data pointer; ignoring (queue is orphaned)."
        );
        return;
    }
    let class: Class = msg![env; av_audio_player class];
    if class == nil {
        log!(
            "Warning: AVAudioPlayer audio callback fired for an object \
             ({:?}) that no longer has a valid class (it was likely \
             deallocated while a buffer was already enqueued); ignoring.",
            av_audio_player
        );
        return;
    }

    let expected_class = env.objc.get_known_class("AVAudioPlayer", &mut env.mem);
    if !env.objc.class_is_subclass_of(class, expected_class) {
        let class_name = env
            .objc
            .try_get_class_name(class)
            .unwrap_or("<unknown>")
            .to_string();
        log!(
            "Warning: AVAudioPlayer audio callback fired on an object of \
             class \"{}\" which is not a subclass of AVAudioPlayer; \
             ignoring (the player was probably deallocated and its \
             memory reused).",
            class_name
        );
        return;
    }

    log_dbg!(
        "_touchHLE_AVAudioPlayerOutputBufferHelper on object of class: {}",
        env.objc.get_class_name(class)
    );
    let &AVAudioPlayerHostObject {
        audio_file_id,
        audio_queue,
        num_packets_to_read,
        current_packet,
        is_playing,
        ..
    } = env.objc.borrow(av_audio_player);
    // The host object might be a phantom (the real one was deallocated
    // while a buffer was still in flight) or it might predate a
    // successful `prepareToPlay`. In either case there's nothing valid we
    // can read from it, so bail out instead of unwrapping None.
    let Some(aq) = audio_queue else {
        log!(
            "Warning: AVAudioPlayer audio callback fired for {:?} which \
             has no audio queue; ignoring.",
            av_audio_player
        );
        return;
    };
    let Some(audio_file_id) = audio_file_id else {
        log!(
            "Warning: AVAudioPlayer audio callback fired for {:?} which \
             has no audio file ID; ignoring.",
            av_audio_player
        );
        return;
    };
    if aq != in_aq {
        log!(
            "Warning: AVAudioPlayer audio callback fired with audio queue \
             {:?} that no longer matches the one stored on the player \
             ({:?}); ignoring.",
            in_aq,
            aq
        );
        return;
    }

    if !is_playing {
        return;
    }

    let num_bytes_ptr: MutPtr<u32> = env.mem.alloc(guest_size_of::<u32>()).cast();
    let num_packets_ptr: MutPtr<u32> = env.mem.alloc(guest_size_of::<u32>()).cast();
    env.mem.write(num_packets_ptr, num_packets_to_read);
    let mut audio_queue_buffer = env.mem.read(in_buf);

    let status = AudioFileReadPackets(
        env,
        audio_file_id,
        false,
        num_bytes_ptr,
        Ptr::null(),
        current_packet,
        num_packets_ptr,
        audio_queue_buffer.audio_data,
    );
    let num_packets = env.mem.read(num_packets_ptr);
    let num_bytes = env.mem.read(num_bytes_ptr);
    env.mem.free(num_packets_ptr.cast());
    env.mem.free(num_bytes_ptr.cast());
    if num_packets > 0 {
        if status != 0 && status != eofErr {
            log!(
                "Warning: AVAudioPlayer read error (status {}), ignoring to prevent crash.",
                status
            );
        }
        audio_queue_buffer.audio_data_byte_size = num_bytes;
        env.mem.write(in_buf, audio_queue_buffer);
        let enqueue_status = AudioQueueEnqueueBuffer(env, aq, in_buf, 0, Ptr::null());

        if enqueue_status != 0 {
            log!(
                "Warning: AudioQueueEnqueueBuffer failed with status {}",
                enqueue_status
            );
        }

        env.objc
            .borrow_mut::<AVAudioPlayerHostObject>(av_audio_player)
            .current_packet = current_packet + num_packets as i64;
    } else {
        // num_packets == 0: end of audio data.
        // Both eofErr and noErr (0) are valid EOF indicators —
        // some platforms return noErr instead of eofErr at end of
        // stream, so we treat them identically.
        if status != 0 && status != eofErr {
            log!(
                "Warning: AVAudioPlayer unexpected read status {}, \
                 treating as EOF.",
                status
            );
        }
        let number_of_loops = env
            .objc
            .borrow::<AVAudioPlayerHostObject>(av_audio_player)
            .num_of_loops;
        if number_of_loops == 0 {
            let stop_status = AudioQueueStop(env, aq, false);
            if stop_status != 0 {
                log!("Warning: AudioQueueStop failed with status {}", stop_status);
            }
            env.objc
                .borrow_mut::<AVAudioPlayerHostObject>(av_audio_player)
                .is_playing = false;
            let delegate = env
                .objc
                .borrow::<AVAudioPlayerHostObject>(av_audio_player)
                .delegate;
            if delegate != nil {
                let successfully: bool = true;
                () = msg![
                    env;
                    delegate
                    audioPlayerDidFinishPlaying:av_audio_player
                    successfully:successfully
                ];
            }
        } else {
            if number_of_loops > 0 {
                env.objc
                    .borrow_mut::<AVAudioPlayerHostObject>(av_audio_player)
                    .num_of_loops -= 1;
            }
            env.objc
                .borrow_mut::<AVAudioPlayerHostObject>(av_audio_player)
                .current_packet = 0;
            // Only recurse if we can actually read data from the
            // start; avoids infinite recursion on broken files.
            let num_packets_to_read = env
                .objc
                .borrow::<AVAudioPlayerHostObject>(av_audio_player)
                .num_packets_to_read;
            if num_packets_to_read > 0 {
                _touchHLE_AVAudioPlayerOutputBufferHelper(env, in_user_data, in_aq, in_buf);
            }
        }
    }
}
