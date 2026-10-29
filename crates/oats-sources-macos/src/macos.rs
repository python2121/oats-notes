//! The Core Audio FFI: every `unsafe` of the crate is in here.

use std::ffi::c_void;
use std::ptr::NonNull;

use oats_sources::Producer;
use objc2_core_audio::{
    AudioObjectGetPropertyData, AudioObjectGetPropertyDataSize, AudioObjectID,
    AudioObjectPropertyAddress, AudioObjectPropertySelector,
    kAudioHardwarePropertyProcessObjectList, kAudioObjectPropertyElementMain,
    kAudioObjectPropertyScopeGlobal, kAudioObjectSystemObject, kAudioProcessPropertyBundleID,
    kAudioProcessPropertyIsRunningOutput, kAudioProcessPropertyPID,
};
use objc2_core_foundation::{CFRetained, CFString};

/// More process objects than this is not believed (a corrupt size).
const MAX_OBJECTS: usize = 4096;

fn address(selector: AudioObjectPropertySelector) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    }
}

/// Read a fixed-size (plain-old-data) property of `object`.
fn read_pod<T: Copy + Default>(
    object: AudioObjectID,
    selector: AudioObjectPropertySelector,
) -> Option<T> {
    let mut addr = address(selector);
    let mut value = T::default();
    let mut size = std::mem::size_of::<T>() as u32;
    // SAFETY: `addr`, `size` and `value` are live locals for the call; Core Audio writes at
    // most `size` bytes into `value`, which is exactly `size_of::<T>()` bytes of a `Copy` POD
    // type (u32 / i32 here), and reports the bytes written back in `size`.
    let status = unsafe {
        AudioObjectGetPropertyData(
            object,
            NonNull::from(&mut addr),
            0,
            std::ptr::null(),
            NonNull::from(&mut size),
            NonNull::from(&mut value).cast::<c_void>(),
        )
    };
    (status == 0 && size as usize == std::mem::size_of::<T>()).then_some(value)
}

/// The process objects Core Audio knows (every process with an audio client); `None` when the
/// property doesn't exist (macOS < 14.2).
pub(crate) fn process_objects() -> Option<Vec<AudioObjectID>> {
    let system = kAudioObjectSystemObject as AudioObjectID;
    let mut addr = address(kAudioHardwarePropertyProcessObjectList);
    let mut size: u32 = 0;
    // SAFETY: `addr` and `size` are live locals; the call only writes the byte size of the
    // property's value into `size`.
    let status = unsafe {
        AudioObjectGetPropertyDataSize(
            system,
            NonNull::from(&mut addr),
            0,
            std::ptr::null(),
            NonNull::from(&mut size),
        )
    };
    if status != 0 {
        return None;
    }
    let count = (size as usize / std::mem::size_of::<AudioObjectID>()).min(MAX_OBJECTS);
    let mut ids: Vec<AudioObjectID> = vec![0; count];
    if count == 0 {
        return Some(ids);
    }
    let mut size = (count * std::mem::size_of::<AudioObjectID>()) as u32;
    // SAFETY: `ids` holds `count` initialised u32s = `size` bytes; Core Audio writes at most
    // `size` bytes into it and reports the bytes written in `size` (the list may have shrunk
    // since the size query, never grown past the buffer we declare).
    let status = unsafe {
        AudioObjectGetPropertyData(
            system,
            NonNull::from(&mut addr),
            0,
            std::ptr::null(),
            NonNull::from(&mut size),
            NonNull::new(ids.as_mut_ptr())?.cast::<c_void>(),
        )
    };
    if status != 0 {
        return None;
    }
    ids.truncate(size as usize / std::mem::size_of::<AudioObjectID>());
    Some(ids)
}

/// The bundle id of a process object (empty for a process without one, e.g. a CLI tool).
fn bundle_id(object: AudioObjectID) -> String {
    let mut addr = address(kAudioProcessPropertyBundleID);
    let mut value: *const CFString = std::ptr::null();
    let mut size = std::mem::size_of::<*const CFString>() as u32;
    // SAFETY: the property's value is a CFStringRef (pointer-sized), written into `value`
    // (a live local of exactly `size` bytes).
    let status = unsafe {
        AudioObjectGetPropertyData(
            object,
            NonNull::from(&mut addr),
            0,
            std::ptr::null(),
            NonNull::from(&mut size),
            NonNull::from(&mut value).cast::<c_void>(),
        )
    };
    if status != 0 {
        return String::new();
    }
    let Some(ptr) = NonNull::new(value.cast_mut()) else {
        return String::new();
    };
    // SAFETY: Core Audio returns the bundle id as a +1 (copied) CFString, which we now own:
    // `CFRetained` releases it when dropped.
    let s = unsafe { CFRetained::from_raw(ptr) };
    s.to_string()
}

/// A producer for `object` when it is producing output right now.
pub(crate) fn producer_if_playing(object: AudioObjectID) -> Option<Producer> {
    let running: u32 = read_pod(object, kAudioProcessPropertyIsRunningOutput)?;
    if running == 0 {
        return None;
    }
    let pid: i32 = read_pod(object, kAudioProcessPropertyPID).unwrap_or(0);
    Some(Producer::from_bundle(pid, &bundle_id(object)))
}
