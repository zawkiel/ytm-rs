//! Optional Windows logon startup for the current user.

use std::os::windows::ffi::OsStrExt;

use windows::Win32::Foundation::ERROR_FILE_NOT_FOUND;
use windows::Win32::System::Registry::{
    HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW,
};
use windows::core::{Result, w};

const RUN: windows::core::PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\Run");
const NAME: windows::core::PCWSTR = w!("YtmRsWindows");

pub fn enabled() -> Result<bool> {
    let mut size = 0;
    // No payload is read. The size query tells us whether the entry exists.
    let status = unsafe {
        RegGetValueW(HKEY_CURRENT_USER, RUN, NAME, RRF_RT_REG_SZ, None, None, Some(&mut size))
    };
    if status == ERROR_FILE_NOT_FOUND {
        return Ok(false);
    }
    status.ok().map(|()| true)
}

pub fn set_enabled(enabled: bool) -> anyhow::Result<()> {
    if !enabled {
        let status = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN, NAME) };
        if status != ERROR_FILE_NOT_FOUND {
            status.ok()?;
        }
        return Ok(());
    }
    let executable = std::env::current_exe()?;
    let quote = u16::from(b'"');
    let command: Vec<u16> = [quote]
        .into_iter()
        .chain(executable.as_os_str().encode_wide())
        .chain([quote, 0])
        .collect();
    // Pass the quoted absolute executable path to the Run key, without a shell.
    unsafe {
        RegSetKeyValueW(
            HKEY_CURRENT_USER, RUN, NAME, REG_SZ.0,
            Some(command.as_ptr().cast()), (command.len() * size_of::<u16>()) as u32,
        )
    }.ok()?;
    Ok(())
}
