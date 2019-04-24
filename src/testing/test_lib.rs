// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

use std::fs::File;
use std::io::Read;
use std::panic::catch_unwind;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Once, ONCE_INIT};

use libmount;
use nix::mount::{umount2, MntFlags};
use uuid::Uuid;

use crate::core::{DevId, DmNameBuf, DmOptions, DmUuidBuf, DM};
use crate::result::{DmError, DmResult, ErrorEnum};

static INIT: Once = ONCE_INIT;
static mut DM_CONTEXT: Option<DM> = None;

fn get_dm() -> &'static DM {
    unsafe {
        INIT.call_once(|| DM_CONTEXT = Some(DM::new().unwrap()));
        match DM_CONTEXT {
            Some(ref context) => context,
            _ => panic!("DM_CONTEXT.is_some()"),
        }
    }
}

/// String that is to be concatenated with test supplied name to identify
/// devices and filesystems generated by tests.
static DM_TEST_ID: &str = "_dm-rs_test_delme";

/// Generate a string with an identifying test suffix
pub fn test_string(name: &str) -> String {
    let mut namestr = String::from(name);
    namestr.push_str(DM_TEST_ID);
    namestr
}

/// Execute command while collecting stdout & stderr.
fn execute_cmd(cmd: &mut Command) -> DmResult<()> {
    match cmd.output() {
        Err(err) => Err(DmError::Dm(
            ErrorEnum::Error,
            format!("cmd: {:?}, error '{}'", cmd, err.to_string()),
        )),
        Ok(result) => {
            if result.status.success() {
                Ok(())
            } else {
                let std_out_txt = String::from_utf8_lossy(&result.stdout);
                let std_err_txt = String::from_utf8_lossy(&result.stderr);
                let err_msg = format!(
                    "cmd: {:?} stdout: {} stderr: {}",
                    cmd, std_out_txt, std_err_txt
                );
                Err(DmError::Dm(ErrorEnum::Error, err_msg))
            }
        }
    }
}

/// Generate an XFS FS, does not specify UUID as that's not supported on version in Travis
pub fn xfs_create_fs(devnode: &Path) -> DmResult<()> {
    execute_cmd(Command::new("mkfs.xfs").arg("-f").arg("-q").arg(&devnode))
}

/// Set a UUID for a XFS volume.
pub fn xfs_set_uuid(devnode: &Path, uuid: &Uuid) -> DmResult<()> {
    execute_cmd(
        Command::new("xfs_admin")
            .arg("-U")
            .arg(format!("{}", uuid))
            .arg(devnode),
    )
}

/// Wait for udev activity to be done.
pub fn udev_settle() -> DmResult<()> {
    execute_cmd(Command::new("udevadm").arg("settle"))
}

/// Generate the test name given the test supplied name.
pub fn test_name(name: &str) -> DmResult<DmNameBuf> {
    DmNameBuf::new(test_string(name))
}

/// Generate the test uuid given the test supplied name.
pub fn test_uuid(name: &str) -> DmResult<DmUuidBuf> {
    DmUuidBuf::new(test_string(name))
}

// For an explanation see:
// https://github.com/rust-lang-nursery/error-chain/issues/254.
// FIXME: Drop dependence on error-chain entirely.
#[allow(deprecated)]
mod cleanup_errors {
    use libmount;
    use nix;
    use std;

    error_chain! {
        foreign_links {
            Ioe(std::io::Error);
            Mnt(libmount::mountinfo::ParseError);
            Nix(nix::Error);
        }
    }
}

use self::cleanup_errors::{Error, Result};

/// Attempt to remove all device mapper devices which match the test naming convention.
/// FIXME: Current implementation complicated by https://bugzilla.redhat.com/show_bug.cgi?id=1506287
fn dm_test_devices_remove() -> Result<()> {
    /// One iteration of removing devicemapper devices
    fn one_iteration() -> Result<(bool, Vec<String>)> {
        let mut progress_made = false;
        let mut remain = Vec::new();

        for n in get_dm()
            .list_devices()
            .map_err(|e| {
                let err_msg = "failed while listing DM devices, giving up";
                Error::with_chain(e, err_msg)
            })?
            .iter()
            .map(|d| &d.0)
            .filter(|n| n.to_string().contains(DM_TEST_ID))
        {
            match get_dm().device_remove(&DevId::Name(n), &DmOptions::new()) {
                Ok(_) => progress_made = true,
                Err(_) => remain.push(n.to_string()),
            }
        }
        Ok((progress_made, remain))
    }

    /// Do one iteration of removals until progress stops. Return remaining
    /// dm devices.
    fn do_while_progress() -> Result<Vec<String>> {
        let mut result = one_iteration()?;
        while result.0 {
            result = one_iteration()?;
        }
        Ok(result.1)
    }

    || -> Result<()> {
        if catch_unwind(get_dm).is_err() {
            return Err("Unable to initialize DM".into());
        }

        do_while_progress().and_then(|remain| {
            if !remain.is_empty() {
                let err_msg = format!("Some test-generated DM devices remaining: {:?}", remain);
                Err(err_msg.into())
            } else {
                Ok(())
            }
        })
    }()
    .map_err(|e| e.chain_err(|| "Failed to ensure removal of all test-generated DM devices"))
}

/// Unmount any filesystems that contain DM_TEST_ID in the mount point.
/// Return immediately on the first unmount failure.
fn dm_test_fs_unmount() -> Result<()> {
    || -> Result<()> {
        let mut mount_data = String::new();
        File::open("/proc/self/mountinfo")?.read_to_string(&mut mount_data)?;
        let parser = libmount::mountinfo::Parser::new(mount_data.as_bytes());

        for mount_point in parser
            .filter_map(|x| x.ok())
            .filter_map(|m| m.mount_point.into_owned().into_string().ok())
            .filter(|mp| mp.contains(DM_TEST_ID))
        {
            umount2(&PathBuf::from(mount_point), MntFlags::MNT_DETACH)?;
        }
        Ok(())
    }()
    .map_err(|e| e.chain_err(|| "Failed to ensure all test-generated filesystems were unmounted"))
}

/// Unmount any filesystems or devicemapper devices which contain DM_TEST_ID
/// in the path or name. Immediately return on first error.
pub(super) fn clean_up() -> Result<()> {
    dm_test_fs_unmount()?;
    dm_test_devices_remove()?;
    Ok(())
}
