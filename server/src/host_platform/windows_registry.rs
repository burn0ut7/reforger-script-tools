//! The Windows registry, where Workbench keeps its own options and where the
//! host records the installed Steam location and the `enfusion` URL handler.
//!
//! Only `HKEY_CURRENT_USER` is written: the extension never changes machine
//! state on the user's behalf.

use std::path::PathBuf;
use windows_sys::Win32::System::Registry::{HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};

/// Reads one string value from `HKEY_CURRENT_USER`, treating a blank value as
/// absent.
pub fn current_user_string(key: &str, value: &str) -> Option<String> {
    current_user_string_including_empty(key, value).filter(|text| !text.trim().is_empty())
}

/// Reads one string value from `HKEY_CURRENT_USER`, including the empty string
/// that marks a registered URL protocol.
pub fn current_user_string_including_empty(key: &str, value: &str) -> Option<String> {
    read_string(HKEY_CURRENT_USER, key, value)
}

/// Every Steam installation root registered on this host.
pub fn steam_roots() -> Vec<PathBuf> {
    [
        (HKEY_CURRENT_USER, r"Software\Valve\Steam", "SteamPath"),
        (
            HKEY_LOCAL_MACHINE,
            r"SOFTWARE\WOW6432Node\Valve\Steam",
            "InstallPath",
        ),
        (HKEY_LOCAL_MACHINE, r"SOFTWARE\Valve\Steam", "InstallPath"),
    ]
    .iter()
    .filter_map(|(hive, key, value)| {
        read_string(*hive, key, value).filter(|text| !text.trim().is_empty())
    })
    .map(PathBuf::from)
    .filter_map(|path| std::fs::canonicalize(path).ok())
    .collect()
}

/// Writes one string value into `HKEY_CURRENT_USER`, reporting whether the
/// registry changed.
pub fn set_current_user_string(
    key_path: &str,
    value_name: Option<&str>,
    value: &str,
) -> std::io::Result<bool> {
    use std::ptr::null_mut;
    use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyW, RegQueryValueExW, RegSetValueExW, HKEY_CURRENT_USER, REG_SZ,
    };

    let key_path = key_path
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let value_name = value_name.map(|name| {
        name.encode_utf16()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>()
    });
    let value_name_ptr = value_name
        .as_ref()
        .map_or(std::ptr::null(), |name| name.as_ptr());
    let mut key = null_mut();
    let status = unsafe { RegCreateKeyW(HKEY_CURRENT_USER, key_path.as_ptr(), &mut key) };
    if status != ERROR_SUCCESS {
        return Err(std::io::Error::from_raw_os_error(status as i32));
    }

    let result = (|| {
        let mut data_type = 0_u32;
        let mut byte_count = 0_u32;
        let read_status = unsafe {
            RegQueryValueExW(
                key,
                value_name_ptr,
                null_mut(),
                &mut data_type,
                null_mut(),
                &mut byte_count,
            )
        };
        let current = if read_status == ERROR_SUCCESS
            && data_type == REG_SZ
            && byte_count >= 2
            && byte_count % 2 == 0
        {
            let mut buffer = vec![0_u16; byte_count as usize / 2];
            let read_status = unsafe {
                RegQueryValueExW(
                    key,
                    value_name_ptr,
                    null_mut(),
                    &mut data_type,
                    buffer.as_mut_ptr().cast(),
                    &mut byte_count,
                )
            };
            if read_status != ERROR_SUCCESS {
                return Err(std::io::Error::from_raw_os_error(read_status as i32));
            }
            Some(
                String::from_utf16_lossy(&buffer)
                    .trim_end_matches('\0')
                    .to_string(),
            )
        } else if read_status == ERROR_FILE_NOT_FOUND {
            None
        } else if read_status == ERROR_SUCCESS && data_type == REG_SZ && byte_count % 2 != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Windows registry string has an odd byte length",
            ));
        } else if read_status == ERROR_SUCCESS {
            None
        } else {
            return Err(std::io::Error::from_raw_os_error(read_status as i32));
        };
        if current.as_deref() == Some(value) {
            return Ok(false);
        }

        let encoded = value
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        let status = unsafe {
            RegSetValueExW(
                key,
                value_name_ptr,
                0,
                REG_SZ,
                encoded.as_ptr().cast(),
                (encoded.len() * std::mem::size_of::<u16>()) as u32,
            )
        };
        if status != ERROR_SUCCESS {
            return Err(std::io::Error::from_raw_os_error(status as i32));
        }
        Ok(true)
    })();
    unsafe { RegCloseKey(key) };
    result
}

fn read_string(hive: HKEY, key: &str, value: &str) -> Option<String> {
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Registry::{RegGetValueW, RRF_RT_REG_SZ};

    let key = key
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let value = value
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let mut byte_count = 0u32;
    let status = unsafe {
        RegGetValueW(
            hive,
            key.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut byte_count,
        )
    };
    if status != ERROR_SUCCESS || byte_count < 2 || byte_count % 2 != 0 {
        return None;
    }

    let mut buffer = vec![0u16; byte_count as usize / 2];
    let status = unsafe {
        RegGetValueW(
            hive,
            key.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            buffer.as_mut_ptr().cast(),
            &mut byte_count,
        )
    };
    if status != ERROR_SUCCESS {
        return None;
    }
    let length = buffer
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(buffer.len());
    String::from_utf16(&buffer[..length]).ok()
}
