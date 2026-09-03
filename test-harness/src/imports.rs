//! The 71 `extern "C"` imports `asr` expects from the auto splitting runtime.
//!
//! Signatures mirror `vendor/asr/src/runtime/sys.rs` exactly, with asr's
//! private newtypes replaced by the primitives they are `repr(transparent)`
//! over: a handle is a `u64`, and an `Option<NonZero…>` is that `u64` with `0`
//! standing for `None`.
//!
//! Anything the splitter does not call is [`unsupported`] rather than a
//! plausible-looking default, so a new dependency on the runtime announces
//! itself.

use crate::{
    timer::{TimerEvent, TimerState},
    with_world, World,
};

/// Panics naming the import that was reached.
fn unsupported(name: &str) -> ! {
    panic!(
        "the splitter called `{name}`, which the harness does not implement. \
         Implement it in harness/src/imports.rs rather than working around it."
    )
}

/// # Safety
///
/// `ptr`/`len` must describe a valid UTF-8 buffer, which asr guarantees.
unsafe fn text(ptr: *const u8, len: usize) -> String {
    if ptr.is_null() || len == 0 {
        return String::new();
    }
    String::from_utf8_lossy(std::slice::from_raw_parts(ptr, len)).into_owned()
}

/// The out-buffer protocol asr uses: `*len_ptr` arrives as the capacity and
/// leaves as the length actually needed. Too small is a `false` return with the
/// needed length written, not a truncated success.
///
/// # Safety
///
/// `buf_ptr` must be writable for `*len_ptr` bytes.
unsafe fn fill(bytes: &[u8], buf_ptr: *mut u8, len_ptr: *mut usize) -> bool {
    let capacity = *len_ptr;
    *len_ptr = bytes.len();
    if bytes.len() > capacity {
        return false;
    }
    std::ptr::copy_nonoverlapping(bytes.as_ptr(), buf_ptr, bytes.len());
    true
}

/// Reads a process handle, or returns `fallback` if it names nothing. Every
/// import taking a handle has to cope with one the test never handed out.
fn with_process<T>(
    world: &World,
    handle: u64,
    fallback: T,
    f: impl FnOnce(&crate::memory::FakeProcess) -> T,
) -> T {
    match world.process(handle) {
        Some(process) => f(process),
        None => fallback,
    }
}

/// Brings a live process's mapping tables up to date. A no-op for a capture,
/// whose mappings are whatever they were when it was taken.
fn refresh(world: &mut World, handle: u64) {
    if let Some(index) = world.process_index(handle) {
        world.processes[index].refresh();
    }
}

// -- timer ------------------------------------------------------------------

#[no_mangle]
pub extern "C" fn timer_get_state() -> u32 {
    with_world(|w| w.timer.state.as_u32())
}

#[no_mangle]
pub extern "C" fn timer_start() {
    with_world(|w| w.timer.record(TimerEvent::Start));
}

#[no_mangle]
pub extern "C" fn timer_split() {
    with_world(|w| w.timer.record(TimerEvent::Split));
}

#[no_mangle]
pub extern "C" fn timer_skip_split() {
    with_world(|w| w.timer.record(TimerEvent::SkipSplit));
}

#[no_mangle]
pub extern "C" fn timer_undo_split() {
    with_world(|w| w.timer.record(TimerEvent::UndoSplit));
}

#[no_mangle]
pub extern "C" fn timer_reset() {
    with_world(|w| w.timer.record(TimerEvent::Reset));
}

#[no_mangle]
pub extern "C" fn timer_current_split_index() -> i64 {
    with_world(|w| match w.timer.state {
        TimerState::NotRunning => -1,
        _ => w.timer.splits() as i64,
    })
}

#[no_mangle]
pub extern "C" fn timer_segment_splitted(idx: u64) -> i32 {
    with_world(|w| i32::from(idx < w.timer.splits() as u64))
}

/// # Safety
///
/// The key and value must be valid UTF-8 buffers.
#[no_mangle]
pub unsafe extern "C" fn timer_set_variable(
    key_ptr: *const u8,
    key_len: usize,
    value_ptr: *const u8,
    value_len: usize,
) {
    let key = text(key_ptr, key_len);
    let value = text(value_ptr, value_len);
    with_world(|w| w.timer.record(TimerEvent::SetVariable { key, value }));
}

#[no_mangle]
pub extern "C" fn timer_set_game_time(secs: i64, nanos: i32) {
    with_world(|w| w.timer.record(TimerEvent::SetGameTime { secs, nanos }));
}

#[no_mangle]
pub extern "C" fn timer_pause_game_time() {
    with_world(|w| w.timer.record(TimerEvent::PauseGameTime));
}

#[no_mangle]
pub extern "C" fn timer_resume_game_time() {
    with_world(|w| w.timer.record(TimerEvent::ResumeGameTime));
}

// -- processes --------------------------------------------------------------

/// # Safety
///
/// The name must be a valid UTF-8 buffer.
#[no_mangle]
pub unsafe extern "C" fn process_attach(name_ptr: *const u8, name_len: usize) -> u64 {
    let name = text(name_ptr, name_len);
    with_world(|w| {
        let Some(index) = w.processes.iter().position(|p| p.open && p.name == name) else {
            return 0;
        };
        w.attached.push(Some(index));
        w.attached.len() as u64
    })
}

#[no_mangle]
pub extern "C" fn process_attach_by_pid(pid: u64) -> u64 {
    with_world(|w| {
        let Some(index) = w.processes.iter().position(|p| p.open && p.pid == pid) else {
            return 0;
        };
        w.attached.push(Some(index));
        w.attached.len() as u64
    })
}

#[no_mangle]
pub extern "C" fn process_detach(process: u64) {
    with_world(|w| {
        if let Some(slot) = process
            .checked_sub(1)
            .and_then(|i| w.attached.get_mut(i as usize))
        {
            *slot = None;
        }
    });
}

/// # Safety
///
/// The name must be valid UTF-8, and `list_ptr` writable for `*list_len_ptr`
/// process ids.
#[no_mangle]
pub unsafe extern "C" fn process_list_by_name(
    name_ptr: *const u8,
    name_len: usize,
    list_ptr: *mut u64,
    list_len_ptr: *mut usize,
) -> bool {
    let name = text(name_ptr, name_len);
    with_world(|w| {
        let pids: Vec<u64> = w
            .processes
            .iter()
            .filter(|p| p.open && p.name == name)
            .map(|p| p.pid)
            .collect();
        let capacity = *list_len_ptr;
        *list_len_ptr = pids.len();
        let writable = pids.len().min(capacity);
        std::ptr::copy_nonoverlapping(pids.as_ptr(), list_ptr, writable);
        true
    })
}

#[no_mangle]
pub extern "C" fn process_is_open(process: u64) -> bool {
    with_world(|w| with_process(w, process, false, |p| p.open))
}

/// # Safety
///
/// `buf_ptr` must be writable for `buf_len` bytes.
#[no_mangle]
pub unsafe extern "C" fn process_read(
    process: u64,
    address: u64,
    buf_ptr: *mut u8,
    buf_len: usize,
) -> bool {
    with_world(|w| {
        with_process(w, process, false, |p| {
            let buf = std::slice::from_raw_parts_mut(buf_ptr, buf_len);
            p.memory.read(address, buf)
        })
    })
}

/// # Safety
///
/// The name must be a valid UTF-8 buffer.
#[no_mangle]
pub unsafe extern "C" fn process_get_module_address(
    process: u64,
    name_ptr: *const u8,
    name_len: usize,
) -> u64 {
    let name = text(name_ptr, name_len);
    with_world(|w| {
        // A module can be loaded after attach; a live process's table has to
        // be current before it is searched.
        refresh(w, process);
        with_process(w, process, 0, |p| {
            p.modules
                .iter()
                .find(|m| m.name == name)
                .map_or(0, |m| m.address)
        })
    })
}

/// # Safety
///
/// The name must be a valid UTF-8 buffer.
#[no_mangle]
pub unsafe extern "C" fn process_get_module_size(
    process: u64,
    name_ptr: *const u8,
    name_len: usize,
) -> u64 {
    let name = text(name_ptr, name_len);
    with_world(|w| {
        // A module can be loaded after attach; a live process's table has to
        // be current before it is searched.
        refresh(w, process);
        with_process(w, process, 0, |p| {
            p.modules
                .iter()
                .find(|m| m.name == name)
                .map_or(0, |m| m.size)
        })
    })
}

/// # Safety
///
/// The name must be valid UTF-8, and the out-buffer must follow asr's protocol.
#[no_mangle]
pub unsafe extern "C" fn process_get_module_path(
    process: u64,
    name_ptr: *const u8,
    name_len: usize,
    buf_ptr: *mut u8,
    buf_len_ptr: *mut usize,
) -> bool {
    let name = text(name_ptr, name_len);
    with_world(|w| {
        with_process(w, process, false, |p| {
            match p
                .modules
                .iter()
                .find(|m| m.name == name)
                .and_then(|m| m.path.as_deref())
            {
                Some(path) => fill(path.as_bytes(), buf_ptr, buf_len_ptr),
                None => false,
            }
        })
    })
}

/// # Safety
///
/// The out-buffer must follow asr's protocol.
#[no_mangle]
pub unsafe extern "C" fn process_get_path(
    process: u64,
    buf_ptr: *mut u8,
    buf_len_ptr: *mut usize,
) -> bool {
    with_world(|w| {
        with_process(w, process, false, |p| match p.path.as_deref() {
            Some(path) => fill(path.as_bytes(), buf_ptr, buf_len_ptr),
            None => false,
        })
    })
}

#[no_mangle]
pub extern "C" fn process_get_memory_range_count(process: u64) -> u64 {
    with_world(|w| {
        // The one place an enumeration begins, so the one place a live
        // process's table can be brought up to date without the indices
        // shifting under whoever is walking it.
        refresh(w, process);
        with_process(w, process, 0, |p| p.ranges.len() as u64)
    })
}

#[no_mangle]
pub extern "C" fn process_get_memory_range_address(process: u64, idx: u64) -> u64 {
    with_world(|w| {
        with_process(w, process, 0, |p| {
            p.ranges.get(idx as usize).map_or(0, |r| r.address)
        })
    })
}

#[no_mangle]
pub extern "C" fn process_get_memory_range_size(process: u64, idx: u64) -> u64 {
    with_world(|w| {
        with_process(w, process, 0, |p| {
            p.ranges.get(idx as usize).map_or(0, |r| r.size)
        })
    })
}

#[no_mangle]
pub extern "C" fn process_get_memory_range_flags(process: u64, idx: u64) -> u64 {
    with_world(|w| {
        with_process(w, process, 0, |p| {
            p.ranges.get(idx as usize).map_or(0, |r| r.flags)
        })
    })
}

// -- runtime ----------------------------------------------------------------

#[no_mangle]
pub extern "C" fn runtime_set_tick_rate(ticks_per_second: f64) {
    with_world(|w| w.tick_rate = Some(ticks_per_second));
}

/// # Safety
///
/// The text must be a valid UTF-8 buffer.
#[no_mangle]
pub unsafe extern "C" fn runtime_print_message(text_ptr: *const u8, text_len: usize) {
    let message = text(text_ptr, text_len);
    with_world(|w| w.log.push(message));
}

/// # Safety
///
/// The out-buffer must follow asr's protocol.
#[no_mangle]
pub unsafe extern "C" fn runtime_get_os(buf_ptr: *mut u8, buf_len_ptr: *mut usize) -> bool {
    with_world(|w| fill(w.os.clone().as_bytes(), buf_ptr, buf_len_ptr))
}

/// # Safety
///
/// The out-buffer must follow asr's protocol.
#[no_mangle]
pub unsafe extern "C" fn runtime_get_arch(buf_ptr: *mut u8, buf_len_ptr: *mut usize) -> bool {
    with_world(|w| fill(w.arch.clone().as_bytes(), buf_ptr, buf_len_ptr))
}

// -- user settings ----------------------------------------------------------

/// # Safety
///
/// The key and description must be valid UTF-8 buffers.
#[no_mangle]
pub unsafe extern "C" fn user_settings_add_bool(
    key_ptr: *const u8,
    key_len: usize,
    _description_ptr: *const u8,
    _description_len: usize,
    default_value: bool,
) -> bool {
    let key = text(key_ptr, key_len);
    with_world(|w| {
        w.registered_settings.push((key.clone(), default_value));
        w.settings.get(&key).copied().unwrap_or(default_value)
    })
}

/// # Safety
///
/// The key and description must be valid UTF-8 buffers.
#[no_mangle]
pub unsafe extern "C" fn user_settings_add_title(
    _key_ptr: *const u8,
    _key_len: usize,
    _description_ptr: *const u8,
    _description_len: usize,
    _heading_level: u32,
) {
}

/// # Safety
///
/// All buffers must be valid UTF-8.
#[no_mangle]
pub unsafe extern "C" fn user_settings_add_choice(
    _key_ptr: *const u8,
    _key_len: usize,
    _description_ptr: *const u8,
    _description_len: usize,
    _default_option_key_ptr: *const u8,
    _default_option_key_len: usize,
) {
    unsupported("user_settings_add_choice")
}

/// # Safety
///
/// All buffers must be valid UTF-8.
#[no_mangle]
pub unsafe extern "C" fn user_settings_add_choice_option(
    _key_ptr: *const u8,
    _key_len: usize,
    _option_key_ptr: *const u8,
    _option_key_len: usize,
    _option_description_ptr: *const u8,
    _option_description_len: usize,
) -> bool {
    unsupported("user_settings_add_choice_option")
}

/// # Safety
///
/// All buffers must be valid UTF-8.
#[no_mangle]
pub unsafe extern "C" fn user_settings_add_file_select(
    _key_ptr: *const u8,
    _key_len: usize,
    _description_ptr: *const u8,
    _description_len: usize,
) {
    unsupported("user_settings_add_file_select")
}

/// # Safety
///
/// All buffers must be valid UTF-8.
#[no_mangle]
pub unsafe extern "C" fn user_settings_add_file_select_name_filter(
    _key_ptr: *const u8,
    _key_len: usize,
    _description_ptr: *const u8,
    _description_len: usize,
    _pattern_ptr: *const u8,
    _pattern_len: usize,
) {
    unsupported("user_settings_add_file_select_name_filter")
}

/// # Safety
///
/// All buffers must be valid UTF-8.
#[no_mangle]
pub unsafe extern "C" fn user_settings_add_file_select_mime_filter(
    _key_ptr: *const u8,
    _key_len: usize,
    _mime_type_ptr: *const u8,
    _mime_type_len: usize,
) {
    unsupported("user_settings_add_file_select_mime_filter")
}

/// # Safety
///
/// All buffers must be valid UTF-8.
#[no_mangle]
pub unsafe extern "C" fn user_settings_set_tooltip(
    _key_ptr: *const u8,
    _key_len: usize,
    _tooltip_ptr: *const u8,
    _tooltip_len: usize,
) {
}

// -- settings map -----------------------------------------------------------
//
// The splitter reads its settings back through the map on every update, so
// enough of this is real for `World::with_setting` to work. Handle 1 is the
// single global map; value handles come from `World::values`.

#[no_mangle]
pub extern "C" fn settings_map_new() -> u64 {
    1
}

#[no_mangle]
pub extern "C" fn settings_map_free(_map: u64) {}

#[no_mangle]
pub extern "C" fn settings_map_load() -> u64 {
    1
}

#[no_mangle]
pub extern "C" fn settings_map_store(_map: u64) {}

#[no_mangle]
pub extern "C" fn settings_map_store_if_unchanged(_old_map: u64, _new_map: u64) -> bool {
    true
}

#[no_mangle]
pub extern "C" fn settings_map_copy(_map: u64) -> u64 {
    1
}

/// # Safety
///
/// The key must be a valid UTF-8 buffer.
#[no_mangle]
pub unsafe extern "C" fn settings_map_insert(
    _map: u64,
    key_ptr: *const u8,
    key_len: usize,
    value: u64,
) {
    let key = text(key_ptr, key_len);
    with_world(|w| {
        if let Some(&boolean) = w.values.get(&value) {
            w.settings.insert(key, boolean);
        }
    });
}

/// # Safety
///
/// The key must be a valid UTF-8 buffer.
#[no_mangle]
pub unsafe extern "C" fn settings_map_get(_map: u64, key_ptr: *const u8, key_len: usize) -> u64 {
    let key = text(key_ptr, key_len);
    with_world(|w| match w.settings.get(&key).copied() {
        Some(boolean) => {
            let handle = w.next_value;
            w.next_value += 1;
            w.values.insert(handle, boolean);
            handle
        }
        None => 0,
    })
}

#[no_mangle]
pub extern "C" fn settings_map_len(_map: u64) -> u64 {
    with_world(|w| w.settings.len() as u64)
}

/// # Safety
///
/// The out-buffer must follow asr's protocol.
#[no_mangle]
pub unsafe extern "C" fn settings_map_get_key_by_index(
    _map: u64,
    _idx: u64,
    _buf_ptr: *mut u8,
    _buf_len_ptr: *mut usize,
) -> bool {
    unsupported("settings_map_get_key_by_index")
}

#[no_mangle]
pub extern "C" fn settings_map_get_value_by_index(_map: u64, _idx: u64) -> u64 {
    unsupported("settings_map_get_value_by_index")
}

// -- settings list ----------------------------------------------------------

#[no_mangle]
pub extern "C" fn settings_list_new() -> u64 {
    unsupported("settings_list_new")
}

#[no_mangle]
pub extern "C" fn settings_list_free(_list: u64) {
    unsupported("settings_list_free")
}

#[no_mangle]
pub extern "C" fn settings_list_copy(_list: u64) -> u64 {
    unsupported("settings_list_copy")
}

#[no_mangle]
pub extern "C" fn settings_list_len(_list: u64) -> u64 {
    unsupported("settings_list_len")
}

#[no_mangle]
pub extern "C" fn settings_list_get(_list: u64, _idx: u64) -> u64 {
    unsupported("settings_list_get")
}

#[no_mangle]
pub extern "C" fn settings_list_push(_list: u64, _value: u64) {
    unsupported("settings_list_push")
}

#[no_mangle]
pub extern "C" fn settings_list_insert(_list: u64, _idx: u64, _value: u64) -> bool {
    unsupported("settings_list_insert")
}

// -- setting values ---------------------------------------------------------

#[no_mangle]
pub extern "C" fn setting_value_new_map(_value: u64) -> u64 {
    unsupported("setting_value_new_map")
}

#[no_mangle]
pub extern "C" fn setting_value_new_list(_value: u64) -> u64 {
    unsupported("setting_value_new_list")
}

#[no_mangle]
pub extern "C" fn setting_value_new_bool(value: bool) -> u64 {
    with_world(|w| {
        let handle = w.next_value;
        w.next_value += 1;
        w.values.insert(handle, value);
        handle
    })
}

#[no_mangle]
pub extern "C" fn setting_value_new_i64(_value: i64) -> u64 {
    unsupported("setting_value_new_i64")
}

#[no_mangle]
pub extern "C" fn setting_value_new_f64(_value: f64) -> u64 {
    unsupported("setting_value_new_f64")
}

/// # Safety
///
/// The value must be a valid UTF-8 buffer.
#[no_mangle]
pub unsafe extern "C" fn setting_value_new_string(_value_ptr: *const u8, _value_len: usize) -> u64 {
    unsupported("setting_value_new_string")
}

#[no_mangle]
pub extern "C" fn setting_value_free(value: u64) {
    with_world(|w| {
        w.values.remove(&value);
    });
}

#[no_mangle]
pub extern "C" fn setting_value_copy(value: u64) -> u64 {
    with_world(|w| match w.values.get(&value).copied() {
        Some(boolean) => {
            let handle = w.next_value;
            w.next_value += 1;
            w.values.insert(handle, boolean);
            handle
        }
        None => 0,
    })
}

#[no_mangle]
pub extern "C" fn setting_value_get_type(_value: u64) -> u32 {
    3 // BOOL; the only kind this harness makes.
}

/// # Safety
///
/// `value_ptr` must be writable.
#[no_mangle]
pub unsafe extern "C" fn setting_value_get_map(_value: u64, _value_ptr: *mut u64) -> bool {
    false
}

/// # Safety
///
/// `value_ptr` must be writable.
#[no_mangle]
pub unsafe extern "C" fn setting_value_get_list(_value: u64, _value_ptr: *mut u64) -> bool {
    false
}

/// # Safety
///
/// `value_ptr` must be writable.
#[no_mangle]
pub unsafe extern "C" fn setting_value_get_bool(value: u64, value_ptr: *mut bool) -> bool {
    with_world(|w| match w.values.get(&value).copied() {
        Some(boolean) => {
            *value_ptr = boolean;
            true
        }
        None => false,
    })
}

/// # Safety
///
/// `value_ptr` must be writable.
#[no_mangle]
pub unsafe extern "C" fn setting_value_get_i64(_value: u64, _value_ptr: *mut i64) -> bool {
    false
}

/// # Safety
///
/// `value_ptr` must be writable.
#[no_mangle]
pub unsafe extern "C" fn setting_value_get_f64(_value: u64, _value_ptr: *mut f64) -> bool {
    false
}

/// # Safety
///
/// The out-buffer must follow asr's protocol.
#[no_mangle]
pub unsafe extern "C" fn setting_value_get_string(
    _value: u64,
    _buf_ptr: *mut u8,
    _buf_len_ptr: *mut usize,
) -> bool {
    false
}
