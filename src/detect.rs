//! Call detection: a call is "another process is using the microphone".
//! Uses CoreAudio process objects (macOS 14.2+), which report per-process input usage,
//! so our own recording never counts as a call.

use std::ffi::c_void;

#[repr(C)]
struct PropertyAddress {
    selector: u32,
    scope: u32,
    element: u32,
}

#[link(name = "CoreAudio", kind = "framework")]
unsafe extern "C" {
    fn AudioObjectGetPropertyDataSize(
        id: u32,
        addr: *const PropertyAddress,
        qualifier_size: u32,
        qualifier: *const c_void,
        out_size: *mut u32,
    ) -> i32;
    fn AudioObjectGetPropertyData(
        id: u32,
        addr: *const PropertyAddress,
        qualifier_size: u32,
        qualifier: *const c_void,
        io_size: *mut u32,
        out: *mut c_void,
    ) -> i32;
}

const fn fourcc(s: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*s)
}

const SYSTEM_OBJECT: u32 = 1;
const PROCESS_OBJECT_LIST: u32 = fourcc(b"prs#");
const PROCESS_PID: u32 = fourcc(b"ppid");
const PROCESS_IS_RUNNING_INPUT: u32 = fourcc(b"piri");

fn addr(selector: u32) -> PropertyAddress {
    PropertyAddress {
        selector,
        scope: fourcc(b"glob"),
        element: 0,
    }
}

fn get<T: Copy + Default>(id: u32, selector: u32) -> Option<T> {
    let mut out = T::default();
    let mut size = size_of::<T>() as u32;
    let status = unsafe {
        AudioObjectGetPropertyData(
            id,
            &addr(selector),
            0,
            std::ptr::null(),
            &mut size,
            (&raw mut out).cast(),
        )
    };
    (status == 0).then_some(out)
}

fn process_objects() -> Vec<u32> {
    let a = addr(PROCESS_OBJECT_LIST);
    let mut size = 0u32;
    if unsafe { AudioObjectGetPropertyDataSize(SYSTEM_OBJECT, &a, 0, std::ptr::null(), &mut size) } != 0 {
        return vec![];
    }
    let mut ids = vec![0u32; size as usize / size_of::<u32>()];
    let status = unsafe {
        AudioObjectGetPropertyData(
            SYSTEM_OBJECT,
            &a,
            0,
            std::ptr::null(),
            &mut size,
            ids.as_mut_ptr().cast(),
        )
    };
    if status != 0 {
        return vec![];
    }
    ids.truncate(size as usize / size_of::<u32>());
    ids
}

fn process_name(pid: i32) -> String {
    let mut buf = [0u8; 256];
    let n = unsafe { libc::proc_name(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
    String::from_utf8_lossy(&buf[..n.max(0) as usize]).into_owned()
}

/// Names of other processes currently capturing from any input device, minus ignored ones
/// (case-insensitive substring match, so "screenpipe" also covers "screenpipe Helper").
pub fn mic_users(ignore: &[String]) -> Vec<String> {
    let me = std::process::id() as i32;
    process_objects()
        .into_iter()
        .filter_map(|obj| {
            // Most processes don't use the mic: check that first, look up the pid only for the rest.
            let running: u32 = get(obj, PROCESS_IS_RUNNING_INPUT)?;
            if running == 0 {
                return None;
            }
            let pid: i32 = get(obj, PROCESS_PID)?;
            if pid == me {
                return None;
            }
            let name = process_name(pid);
            let lower = name.to_lowercase();
            (!ignore.iter().any(|i| lower.contains(&i.to_lowercase()))).then_some(name)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn process_list_is_readable() {
        assert!(
            !super::process_objects().is_empty(),
            "CoreAudio returned no process objects"
        );
        eprintln!("mic users now: {:?}", super::mic_users(&[]));
    }
}
